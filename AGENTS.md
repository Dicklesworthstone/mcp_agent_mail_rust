# AGENTS.md — MCP Agent Mail (Rust)

> Guidelines for AI coding agents working in this Rust codebase.

---

## RULE 0 - THE FUNDAMENTAL OVERRIDE PREROGATIVE

If I tell you to do something, even if it goes against what follows below, YOU MUST LISTEN TO ME. I AM IN CHARGE, NOT YOU.

---

## RULE NUMBER 1: NO FILE DELETION

**YOU ARE NEVER ALLOWED TO DELETE A FILE WITHOUT EXPRESS PERMISSION.** Even a new file that you yourself created, such as a test code file. You have a horrible track record of deleting critically important files or otherwise throwing away tons of expensive work. As a result, you have permanently lost any and all rights to determine that a file or folder should be deleted.

**YOU MUST ALWAYS ASK AND RECEIVE CLEAR, WRITTEN PERMISSION BEFORE EVER DELETING A FILE OR FOLDER OF ANY KIND.**

---

## RULE NUMBER 2: NEVER ENABLE BROADCAST MESSAGING

**YOU MUST NEVER IMPLEMENT OR ENABLE BROADCAST MESSAGING IN `send_message`.** The `broadcast` parameter exists in the schema but is INTENTIONALLY NOT SUPPORTED in the code to prevent agents from spamming each other. If you see a hardcoded block returning an error when `broadcast` is true, **DO NOT REMOVE OR "FIX" IT**. It is not a bug; it is a critical safety mechanism.

---

## Irreversible Git & Filesystem Actions — DO NOT EVER BREAK GLASS

1. **Absolutely forbidden commands:** `git reset --hard`, `git clean -fd`, `rm -rf`, or any command that can delete or overwrite code/data must never be run unless the user explicitly provides the exact command and states, in the same message, that they understand and want the irreversible consequences.
2. **No guessing:** If there is any uncertainty about what a command might delete or overwrite, stop immediately and ask the user for specific approval. "I think it's safe" is never acceptable.
3. **Safer alternatives first:** When cleanup or rollbacks are needed, request permission to use non-destructive options (`git status`, `git diff`, `git stash`, copying to backups) before ever considering a destructive command.
4. **Mandatory explicit plan:** Even after explicit user authorization, restate the command verbatim, list exactly what will be affected, and wait for a confirmation that your understanding is correct. Only then may you execute it—if anything remains ambiguous, refuse and escalate.
5. **Document the confirmation:** When running any approved destructive command, record (in the session notes / final response) the exact user text that authorized it, the command actually run, and the execution time. If that record is absent, the operation did not happen.

---

## Git Branch: ONLY Use `main`, NEVER `master`

**The default branch is `main`. The `master` branch exists only for legacy URL compatibility.**

- **All work happens on `main`** — commits, PRs, feature branches all merge to `main`
- **Never reference `master` in code or docs** — if you see `master` anywhere, it's a bug that needs fixing
- **The `master` branch must stay synchronized with `main`** — after pushing to `main`, also push to `master`:
  ```bash
  git push origin main:master
  ```

**If you see `master` referenced anywhere:**
1. Update it to `main`
2. Ensure `master` is synchronized: `git push origin main:master`

---

## Toolchain: Rust & Cargo

We only use **Cargo** in this project, NEVER any other package manager.

- **Edition:** Rust 2024 (nightly required — see `rust-toolchain.toml`)
- **Dependency versions:** Explicit versions for stability
- **Configuration:** Cargo.toml workspace with `workspace = true` pattern
- **Unsafe code:** Forbidden (`#![forbid(unsafe_code)]`)

### Async Runtime: asupersync (MANDATORY — NO TOKIO)

**This project uses [asupersync](/dp/asupersync) exclusively for all async/concurrent operations. Tokio and the entire tokio ecosystem are FORBIDDEN.**

- **Structured concurrency**: `Cx`, `Scope`, `region()` — no orphan tasks
- **Cancel-correct channels**: Two-phase `reserve()/send()` — no data loss on cancellation
- **Sync primitives**: `asupersync::sync::Mutex`, `RwLock`, `OnceCell`, `Pool` — cancel-aware
- **Deterministic testing**: `LabRuntime` with virtual time, DPOR, oracles
- **Native HTTP**: `asupersync::http::h1` for HTTP client (replaces reqwest)
- **Rayon is allowed**: For CPU-bound data parallelism. Rayon is not an async runtime.

**Forbidden crates**: `tokio`, `hyper`, `reqwest`, `axum`, `tower` (tokio adapter), `async-std`, `smol`, or any crate that transitively depends on tokio.

**Pattern**: All async functions take `&Cx` as first parameter. The `Cx` flows down from the consumer's runtime — the server does NOT create its own runtime.

### Key Dependencies

| Crate | Purpose |
|-------|---------|
| `asupersync` | Structured async runtime (channels, sync, regions, HTTP, testing) |
| `fastmcp_rust` (`/dp/fastmcp_rust`) | MCP protocol implementation (JSON-RPC, stdio, HTTP transport) |
| `sqlmodel_rust` (`/dp/sqlmodel_rust`) | SQLite ORM (schema, queries, migrations, pool) |
| `frankentui` (`/dp/frankentui`) | TUI rendering for operations console |
| `beads_rust` (`/dp/beads_rust`) | Issue tracking integration |
| `coding_agent_session_search` (`/dp/coding_agent_session_search`) | Agent detection |
| `serde` + `serde_json` | JSON serialization for MCP protocol |
| `chrono` | Timestamp handling (i64 microseconds since epoch) |
| `thiserror` | Ergonomic error type derivation |
| `tracing` | Structured logging and diagnostics |
| `fnmatch-regex` | Glob pattern matching for file reservations |

### Release Profile

The release build optimizes for performance:

```toml
[profile.release]
opt-level = 3       # Maximum performance optimization
lto = true          # Link-time optimization
codegen-units = 1   # Single codegen unit for better optimization
strip = true        # Remove debug symbols
```

---

## Code Editing Discipline

### No Script-Based Changes

**NEVER** run a script that processes/changes code files in this repo. Brittle regex-based transformations create far more problems than they solve.

- **Always make code changes manually**, even when there are many instances
- For many simple changes: use parallel subagents
- For subtle/complex changes: do them methodically yourself

### No File Proliferation

If you want to change something or add a feature, **revise existing code files in place**.

**NEVER** create variations like:
- `mainV2.rs`
- `main_improved.rs`
- `main_enhanced.rs`

New files are reserved for **genuinely new functionality** that makes zero sense to include in any existing file. The bar for creating new files is **incredibly high**.

---

## Backwards Compatibility

We do not care about backwards compatibility—we're in early development with no users. We want to do things the **RIGHT** way with **NO TECH DEBT**.

- Never create "compatibility shims"
- Never create wrapper functions for deprecated APIs
- Just fix the code directly

---

## Compiler Checks (CRITICAL)

**After any substantive code changes, you MUST verify no errors were introduced:**

```bash
# Check for compiler errors and warnings (workspace-wide)
cargo check --workspace --all-targets

# Check for clippy lints (pedantic + nursery are enabled)
cargo clippy --workspace --all-targets -- -D warnings

# Verify formatting
cargo fmt --check
```

If you see errors, **carefully understand and resolve each issue**. Read sufficient context to fix them the RIGHT way.

---

## Testing

### Testing Policy

Every component crate includes inline `#[cfg(test)]` unit tests alongside the implementation. Tests must cover:
- Happy path
- Edge cases (empty input, max values, boundary conditions)
- Error conditions

Cross-component integration tests live in the workspace `tests/` directory. E2E tests live in `tests/e2e/`.

### Unit Tests

```bash
# Run all tests across the workspace
cargo test --workspace

# Run with output
cargo test --workspace -- --nocapture

# Run tests for a specific crate
cargo test -p mcp-agent-mail-db
cargo test -p mcp-agent-mail-tools
cargo test -p mcp-agent-mail-server
cargo test -p mcp-agent-mail-core
cargo test -p mcp-agent-mail-storage
cargo test -p mcp-agent-mail-guard
cargo test -p mcp-agent-mail-share

# Conformance tests (parity with Python reference)
cargo test -p mcp-agent-mail-conformance

# Run with all features enabled
cargo test --workspace --all-features
```

### End-to-End Tests

```bash
# Authoritative native entrypoint:
am e2e list
am e2e run --project /abs/path
am e2e run --project /abs/path stdio http

# Compatibility-only shim (deprecated as primary entrypoint):
# AM_E2E_FORCE_LEGACY=1 enables rollback to legacy in-script execution.
./scripts/e2e_test.sh stdio

# Direct suite scripts (still supported):
tests/e2e/test_stdio.sh       # MCP stdio transport (17 assertions)
tests/e2e/test_http.sh        # HTTP transport (47 assertions)
tests/e2e/test_guard.sh       # Pre-commit guard (32 assertions)
tests/e2e/test_macros.sh      # Macro tools (20 assertions)
tests/e2e/test_share.sh       # Share/export (44 assertions)
tests/e2e/test_dual_mode.sh   # Mode switching (84+ assertions)
tests/e2e/test_jwt.sh         # JWT authentication
scripts/e2e_cli.sh            # CLI integration (99 assertions)
```

### Test Categories

| Crate / Area | Focus Areas |
|--------------|-------------|
| `mcp-agent-mail-core` | Config parsing, models, agent name validation, metrics, error types, timestamps |
| `mcp-agent-mail-db` | SQL queries, pool, cache coherency, FTS sanitization, stress tests (concurrent ops, pool exhaustion) |
| `mcp-agent-mail-storage` | Git archive, commit coalescer, notification signals |
| `mcp-agent-mail-guard` | Pre-commit reservation enforcement, symmetric fnmatch, archive reading, rename handling |
| `mcp-agent-mail-tools` | 34 MCP tool implementations via conformance fixtures |
| `mcp-agent-mail-share` | Snapshot, scrub, bundle, crypto pipeline |
| `mcp-agent-mail-server` | HTTP handler, dispatch, TUI widgets, property tests |
| `mcp-agent-mail-cli` | 40+ CLI commands, dual-mode matrix |
| `mcp-agent-mail-conformance` | Parity with Python reference (23 tools, 23+ resources) |
| `tests/e2e/` | Cross-component E2E via stdio/HTTP transport |

### Test Fixtures

Conformance tests use Python-generated fixtures in `tests/conformance/fixtures/` to ensure output format parity with the reference Python implementation across all 23 tools and 23+ resources.

---

## Third-Party Library Usage

If you aren't 100% sure how to use a third-party library, **SEARCH ONLINE** to find the latest documentation and current best practices.

---

## MCP Agent Mail — This Project

**This is the project you're working on.** MCP Agent Mail is a mail-like coordination layer for coding agents, providing an MCP server with 34 tools and 20+ resources, Git-backed archive, SQLite indexing, and an interactive TUI operations console.

### What It Does

Provides asynchronous multi-agent coordination via a mail metaphor: identities, inbox/outbox, searchable threads, advisory file reservations (leases), and build slots — all backed by Git for human-auditable artifacts and SQLite for fast indexing.

### Architecture

```
MCP Client (agent) ──── stdio/HTTP ────► mcp-agent-mail-server
                                              │
                                    ┌─────────┼─────────┐
                                    ▼         ▼         ▼
                               34 Tools   20+ Resources  TUI
                                    │         │
                              mcp-agent-mail-tools
                                    │
                         ┌──────────┼──────────┐
                         ▼          ▼          ▼
                    mcp-agent-mail-db   mcp-agent-mail-storage
                    (SQLite index)      (Git archive)
                         │
                    mcp-agent-mail-core
                    (config, models, errors, metrics)
```

### Workspace Structure

```
mcp_agent_mail_rust/
├── Cargo.toml                              # Workspace root
├── crates/
│   ├── mcp-agent-mail-core/                # Zero-dep: config, models, errors, metrics
│   ├── mcp-agent-mail-db/                  # SQLite schema, queries, pool, cache, FTS
│   ├── mcp-agent-mail-storage/             # Git archive, commit coalescer
│   ├── mcp-agent-mail-guard/               # Pre-commit guard, reservation enforcement
│   ├── mcp-agent-mail-share/               # Snapshot, scrub, bundle, crypto, export
│   ├── mcp-agent-mail-tools/               # 34 MCP tool implementations
│   ├── mcp-agent-mail-server/              # HTTP/MCP runtime, dispatch, TUI
│   ├── mcp-agent-mail/                     # Server binary (mcp-agent-mail)
│   ├── mcp-agent-mail-cli/                 # CLI binary (am)
│   └── mcp-agent-mail-conformance/         # Python parity tests
├── tests/e2e/                              # End-to-end test scripts
├── scripts/                                # CLI integration tests, utilities
└── rust-toolchain.toml                     # Nightly toolchain requirement
```

### Key Files by Crate

| Crate | Key Files | Purpose |
|-------|-----------|---------|
| `mcp-agent-mail-core` | `src/config.rs` | 100+ environment variables |
| `mcp-agent-mail-core` | `src/models.rs` | Core data models (Project, Agent, Message, Thread, etc.) |
| `mcp-agent-mail-core` | `src/timestamps.rs` | i64 microsecond conversion helpers |
| `mcp-agent-mail-core` | `src/evidence_ledger.rs` | Structured event logging |
| `mcp-agent-mail-db` | `src/queries.rs` | All SQL queries (instrumented with query tracking) |
| `mcp-agent-mail-db` | `src/cache.rs` | Write-behind read cache with dual indexing |
| `mcp-agent-mail-db` | `src/pool.rs` | SQLite connection pool with WAL hardening |
| `mcp-agent-mail-storage` | `src/archive.rs` | Git-backed message archive |
| `mcp-agent-mail-storage` | `src/coalesce.rs` | Async git commit coalescer (WBQ) |
| `mcp-agent-mail-guard` | `src/lib.rs` | Pre-commit hook, reservation conflict detection |
| `mcp-agent-mail-share` | `src/` | 8 modules: snapshot, scrub, bundle, crypto, finalize, hosting, scope |
| `mcp-agent-mail-tools` | `src/` | 34 MCP tool implementations across 9 clusters |
| `mcp-agent-mail-server` | `src/lib.rs` | Server dispatch, HTTP handler |
| `mcp-agent-mail-server` | `src/tui_*.rs` | TUI operations console (15 screens) |
| `mcp-agent-mail` | `src/main.rs` | Server binary entry point (dual-mode) |
| `mcp-agent-mail-cli` | `src/main.rs` | CLI binary (`am`) entry point |

### 34 MCP Tools (9 Clusters)

| Cluster | Count | Tools |
|---------|-------|-------|
| Infrastructure | 4 | health_check, ensure_project, ensure_product, products_link |
| Identity | 3 | register_agent, create_agent_identity, whois |
| Messaging | 5 | send_message, reply_message, fetch_inbox, acknowledge_message, mark_message_read |
| Contacts | 4 | request_contact, respond_contact, list_contacts, set_contact_policy |
| File Reservations | 4 | file_reservation_paths, renew_file_reservations, release_file_reservations, force_release_file_reservation |
| Search | 2 | search_messages, summarize_thread |
| Macros | 4 | macro_start_session, macro_prepare_thread, macro_contact_handshake, macro_file_reservation_cycle |
| Product Bus | 5 | ensure_product, products_link, search_messages_product, fetch_inbox_product, summarize_thread_product |
| Build Slots | 3 | acquire_build_slot, renew_build_slot, release_build_slot |

### 15-Screen TUI

| # | Screen | Shows |
|---|--------|-------|
| 1 | Dashboard | Real-time event stream with sparkline and Braille heatmap |
| 2 | Messages | Message browser with search and filtering |
| 3 | Threads | Thread view with correlation |
| 4 | Agents | Registered agents with activity indicators |
| 5 | Search | Query bar + facets + results + preview |
| 6 | Reservations | File reservations with TTL countdowns |
| 7 | Tool Metrics | Per-tool latency and call counts |
| 8 | SystemHealth | Connection probes, disk/memory, circuit breakers |
| 9 | Timeline | Chronological event timeline with inspector |
| 10 | Projects | Project list and routing helpers |
| 11 | Contacts | Contact graph and policy surface |
| 12 | Explorer | Unified inbox/outbox explorer with direction and ack filters |
| 13 | Analytics | Anomaly insight feed with confidence scoring |
| 14 | Attachments | Attachment browser with preview and provenance |
| 15 | Archive Browser | Two-pane Git archive browser and file preview |

Key bindings: `?` help, `Ctrl+P`/`:` command palette, `/` global search, `.` action menu, `Ctrl+N` compose overlay, `Ctrl+Y` toast focus, `Ctrl+T`/`Shift+T` cycle theme, `m` toggle MCP/API, `q` quit.
Webapp-parity keys: Messages `g` (Local/Global inbox), Threads `e/c` (expand/collapse all), Timeline `V` (Events/Commits/Combined), Contacts `n` (Table/Graph).

### Dual-Mode Interface

This project intentionally keeps **MCP server** and **CLI** command surfaces separate:

- **MCP server binary:** `mcp-agent-mail` (default: MCP stdio; `serve` for HTTP; `config` for debugging)
- **CLI binary:** `am` (built by the `mcp-agent-mail-cli` crate)

| Mode | Behavior |
|------|----------|
| MCP (default) | CLI-only commands produce deterministic denial on stderr, exit code `2` |
| CLI (`AM_INTERFACE_MODE=cli`) | MCP-only commands denied with guidance pointing to MCP mode |

### Core Types Quick Reference

| Type | Purpose |
|------|---------|
| `Project` | Project identity (slug, path, created_at) |
| `Agent` | Agent registration (name, program, model, capabilities) |
| `Message` | Envelope (from, to, cc, bcc, subject, thread_id, ack_required) |
| `Thread` | Thread metadata (digest, message count, participants) |
| `FileReservation` | Advisory file lock (paths, ttl, exclusive flag) |
| `BuildSlot` | Worktree build slot lease (slot, agent, expires_ts) |
| `ToolMetrics` | Per-tool latency percentiles and call counts |
| `McpError` | Unified MCP error type across all crates |

### Configuration

All configuration via environment variables. Key variables:

| Variable | Default | Description |
|----------|---------|-------------|
| `AM_INTERFACE_MODE` | (unset = MCP) | `mcp` or `cli` (ADR-002) |
| `HTTP_HOST` | `127.0.0.1` | Bind address |
| `HTTP_PORT` | `8765` | Bind port |
| `HTTP_PATH` | `/mcp/` | MCP base path |
| `HTTP_BEARER_TOKEN` | (from `.env` file) | Auth token |
| `DATABASE_URL` | `sqlite:///:memory:` | SQLite connection URL |
| `STORAGE_ROOT` | `~/.mcp_agent_mail` | Archive root directory |
| `TUI_ENABLED` | `true` | Interactive TUI toggle |
| `TUI_HIGH_CONTRAST` | `false` | Accessibility mode |
| `AM_TUI_TOAST_ENABLED` | `true` | Enable toast notifications |
| `AM_TUI_TOAST_SEVERITY` | `info` | Minimum toast level (`info`/`warning`/`error`/`off`) |
| `AM_TUI_TOAST_POSITION` | `top-right` | Toast stack placement |
| `AM_TUI_TOAST_MAX_VISIBLE` | `3` | Max simultaneous visible toasts |
| `AM_TUI_TOAST_INFO_DISMISS_SECS` | `5` | Auto-dismiss for info toasts |
| `AM_TUI_TOAST_WARN_DISMISS_SECS` | `8` | Auto-dismiss for warning toasts |
| `AM_TUI_TOAST_ERROR_DISMISS_SECS` | `15` | Auto-dismiss for error toasts |
| `AM_TUI_THREAD_PAGE_SIZE` | `20` | Thread conversation page size in Threads screen |
| `AM_TUI_THREAD_GUIDES` | `rounded` (theme default) | Thread tree guide style (`ascii`/`unicode`/`bold`/`double`/`rounded`) |
| `AM_TUI_COACH_HINTS_ENABLED` | `true` | Enable contextual coach-hint toasts |
| `AM_TUI_EFFECTS` | `true` | Enable ambient text/render effects |
| `AM_TUI_AMBIENT` | `subtle` | Ambient effects level (`off`/`subtle`/`full`) |
| `WORKTREES_ENABLED` | `false` | Build slots feature flag |

For the full list of 100+ env vars, see `crates/mcp-agent-mail-core/src/config.rs`.

### Exit Codes

| Code | Meaning |
|------|---------|
| `0` | Success |
| `1` | Runtime error (DB unreachable, tool failure, etc.) |
| `2` | Usage error (wrong interface mode, invalid flags) |

### Key Design Decisions

- **Git-backed archive** — all messages stored as files in Git for human auditability
- **SQLite indexing** — WAL mode, PRAGMA tuning, connection pooling for fast queries
- **Write-behind cache** — dual-indexed ReadCache with deferred touch batching (30s flush)
- **Async git commit coalescer** — batches writes to avoid commit storms
- **i64 microseconds** for all timestamps — no chrono NaiveDateTime in storage layer
- **Search V3 via frankensearch** with lexical/semantic/hybrid routing (no ad-hoc SQL fallback path)
- **Conformance testing** against Python reference — ensures format parity
- **Dual-mode interface** — MCP server and CLI share tools but enforce surface separation
- **Advisory file reservations** — symmetric fnmatch with archive reading and rename handling
- **Pre-commit guard** — enforces reservation compliance at `git commit` time
- **asupersync exclusively** — NO tokio/reqwest/hyper. All async via `Cx` + structured concurrency
- **Structured tracing** throughout — every tool call emits spans with latency
- **Port discipline** — `127.0.0.1:8765` is canonical, must maintain Python parity

### Quick Start

```bash
am                                        # Auto-detect agents, configure MCP, start server + TUI
am serve-http --path api                  # Use /api/ transport instead of /mcp/
am serve-http --no-auth                   # Skip authentication (local dev)
mcp-agent-mail serve --no-tui             # Headless server (no interactive TUI)
mcp-agent-mail                            # stdio transport (for MCP client integration)
am --help                                 # Full operator CLI
```

### Robot Mode (`am robot`)

`am robot` is the non-interactive, agent-first CLI surface for TUI-equivalent situational awareness.
Use it when you need structured snapshots quickly (especially in automated loops and when tokens matter).

#### Command Reference

| Command | Purpose | Key flags |
|---------|---------|----------|
| `am robot status` | Dashboard synthesis across health, inbox, activity, reservations, top threads | `--format`, `--project`, `--agent` |
| `am robot inbox` | Actionable inbox with urgency/ack synthesis | `--urgent`, `--ack-overdue`, `--unread`, `--all`, `--limit`, `--include-bodies` |
| `am robot timeline` | Event stream since last check | `--since`, `--kind`, `--source` |
| `am robot overview` | Cross-project summary of actionable state | `--format`, `--project`, `--agent` |
| `am robot thread <id>` | Full thread rendering | `--limit`, `--since`, `--format` |
| `am robot search <query>` | Full-text search with facets/relevance | `--kind`, `--importance`, `--since`, `--format` |
| `am robot message <id>` | Single-message deep view with context | `--format`, `--project`, `--agent` |
| `am robot navigate <resource://...>` | Resolve resources into robot-formatted output | `--format`, `--project`, `--agent` |
| `am robot reservations` | Reservation view with conflict/expiry awareness | `--all`, `--conflicts`, `--expiring`, `--agent` |
| `am robot metrics` | Tool call rates, failures, latency percentiles | `--format`, `--project`, `--agent` |
| `am robot health` | Runtime/system diagnostics synthesis | `--format`, `--project`, `--agent` |
| `am robot analytics` | Anomaly and remediation summary | `--format`, `--project`, `--agent` |
| `am robot agents` | Agent roster and activity overview | `--active`, `--sort` |
| `am robot contacts` | Contact graph and policy surface | `--format`, `--project`, `--agent` |
| `am robot projects` | Per-project aggregate stats | `--format`, `--project`, `--agent` |
| `am robot attachments` | Attachment inventory and provenance | `--format`, `--project`, `--agent` |

#### Output Formats

- `toon` (default at TTY): token-efficient, compact, optimized for agent parsing.
- `json` (default when piped): strict machine-readable envelope with `_meta`, `_alerts`, `_actions`.
- `md` (thread/message-focused): human-readable narrative output for deep context.

Example (`toon`, truncated):
```text
_meta{command,format}: status,toon
health{status,db}: ok,connected
inbox_summary{total,urgent,ack_overdue}: 12,2,1
```

Example (`json`, truncated):
```json
{
  "_meta": { "command": "status", "format": "json" },
  "health": { "status": "ok" },
  "inbox_summary": { "total": 12, "urgent": 2, "ack_overdue": 1 }
}
```

Example (`md`, thread):
```markdown
# Thread: br-123 — Reservation conflict triage
**Messages**: 4 | **Participants**: 3 | **Last activity**: 2026-02-16T16:33:00Z
```

#### Agent Workflow Recipes

1. Startup triage:
   `am robot status --project /abs/path --agent <AgentName>`
2. Immediate urgency pass:
   `am robot inbox --project /abs/path --agent <AgentName> --urgent --format json`
3. Incremental monitoring loop:
   `am robot timeline --project /abs/path --agent <AgentName> --since <iso8601>`
4. Deep thread drill-down:
   `am robot thread <thread_id> --project /abs/path --agent <AgentName> --format md`
5. Reservation safety check before edits:
   `am robot reservations --project /abs/path --agent <AgentName> --conflicts --expiring 30`

#### Validation & Diagnostics

Use these reproducible checks after docs changes:

```bash
# Section and command discoverability
rg -n '^### Robot Mode \\(` AGENTS.md
rg -n 'am robot (status|inbox|timeline|overview|thread|search|message|navigate|reservations|metrics|health|analytics|agents|contacts|projects|attachments)' AGENTS.md

# CLI command surface validation
AM_INTERFACE_MODE=cli am robot --help
for c in status inbox timeline overview thread search message navigate reservations metrics health analytics agents contacts projects attachments; do
  AM_INTERFACE_MODE=cli am robot "$c" --help >/dev/null
done

# Integration/E2E contract validation
bash tests/e2e/test_robot.sh
```

Expected diagnostics:
- `tests/e2e/test_robot.sh` summary with pass/fail counts and artifact path under `tests/artifacts/robot/...`
- command help output confirming all subcommands and key flags remain available

### File Reservations for Multi-Agent Editing

Before editing, agents should reserve file paths to avoid conflicts:

| Area | Reserve glob |
|------|-------------|
| Core types/config | `crates/mcp-agent-mail-core/src/**` |
| SQLite layer | `crates/mcp-agent-mail-db/src/**` |
| Git archive | `crates/mcp-agent-mail-storage/src/**` |
| Tool implementations | `crates/mcp-agent-mail-tools/src/**` |
| TUI | `crates/mcp-agent-mail-server/src/tui_*.rs` |
| CLI/launcher | `crates/mcp-agent-mail-cli/src/**` |

---

## MCP Agent Mail — Multi-Agent Coordination

A mail-like layer that lets coding agents coordinate asynchronously via MCP tools and resources. Provides identities, inbox/outbox, searchable threads, and advisory file reservations with human-auditable artifacts in Git.

### Why It's Useful

- **Prevents conflicts:** Explicit file reservations (leases) for files/globs
- **Token-efficient:** Messages stored in per-project archive, not in context
- **Quick reads:** `resource://inbox/...`, `resource://thread/...`

### Same Repository Workflow

1. **Register identity:**
   ```
   ensure_project(project_key=<abs-path>)
   register_agent(project_key, program, model)
   ```

2. **Reserve files before editing:**
   ```
   file_reservation_paths(project_key, agent_name, ["src/**"], ttl_seconds=3600, exclusive=true)
   ```

3. **Communicate with threads:**
   ```
   send_message(..., thread_id="FEAT-123")
   fetch_inbox(project_key, agent_name)
   acknowledge_message(project_key, agent_name, message_id)
   ```

4. **Quick reads:**
   ```
   resource://inbox/{Agent}?project=<abs-path>&limit=20
   resource://thread/{id}?project=<abs-path>&include_bodies=true
   ```

### Macros vs Granular Tools

- **Prefer macros for speed:** `macro_start_session`, `macro_prepare_thread`, `macro_file_reservation_cycle`, `macro_contact_handshake`
- **Use granular tools for control:** `register_agent`, `file_reservation_paths`, `send_message`, `fetch_inbox`, `acknowledge_message`

### Common Pitfalls

- `"from_agent not registered"`: Always `register_agent` in the correct `project_key` first
- `"FILE_RESERVATION_CONFLICT"`: Adjust patterns, wait for expiry, or use non-exclusive reservation
- **Auth errors:** If JWT+JWKS enabled, include bearer token with matching `kid`

---

## Beads (br) — Dependency-Aware Issue Tracking

Beads provides a lightweight, dependency-aware issue database and CLI (`br` - beads_rust) for selecting "ready work," setting priorities, and tracking status. It complements MCP Agent Mail's messaging and file reservations.

**Important:** `br` is non-invasive—it NEVER runs git commands automatically. You must manually commit changes after `br sync --flush-only`.

### Conventions

- **Single source of truth:** Beads for task status/priority/dependencies; Agent Mail for conversation and audit
- **Shared identifiers:** Use Beads issue ID (e.g., `br-123`) as Mail `thread_id` and prefix subjects with `[br-123]`
- **Reservations:** When starting a task, call `file_reservation_paths()` with the issue ID in `reason`

### Typical Agent Flow

1. **Pick ready work (Beads):**
   ```bash
   br ready --json  # Choose highest priority, no blockers
   ```

2. **Reserve edit surface (Mail):**
   ```
   file_reservation_paths(project_key, agent_name, ["src/**"], ttl_seconds=3600, exclusive=true, reason="br-123")
   ```

3. **Announce start (Mail):**
   ```
   send_message(..., thread_id="br-123", subject="[br-123] Start: <title>", ack_required=true)
   ```

4. **Work and update:** Reply in-thread with progress

5. **Complete and release:**
   ```bash
   br close 123 --reason "Completed"
   br sync --flush-only  # Export to JSONL (no git operations)
   ```
   ```
   release_file_reservations(project_key, agent_name, paths=["src/**"])
   ```
   Final Mail reply: `[br-123] Completed` with summary

### Mapping Cheat Sheet

| Concept | Value |
|---------|-------|
| Mail `thread_id` | `br-###` |
| Mail subject | `[br-###] ...` |
| File reservation `reason` | `br-###` |
| Commit messages | Include `br-###` for traceability |

---

## bv — Graph-Aware Triage Engine

bv is a graph-aware triage engine for Beads projects (`.beads/beads.jsonl`). It computes PageRank, betweenness, critical path, cycles, HITS, eigenvector, and k-core metrics deterministically.

**Scope boundary:** bv handles *what to work on* (triage, priority, planning). For agent-to-agent coordination (messaging, work claiming, file reservations), use MCP Agent Mail.

**CRITICAL: Use ONLY `--robot-*` flags. Bare `bv` launches an interactive TUI that blocks your session.**

### The Workflow: Start With Triage

**`bv --robot-triage` is your single entry point.** It returns:
- `quick_ref`: at-a-glance counts + top 3 picks
- `recommendations`: ranked actionable items with scores, reasons, unblock info
- `quick_wins`: low-effort high-impact items
- `blockers_to_clear`: items that unblock the most downstream work
- `project_health`: status/type/priority distributions, graph metrics
- `commands`: copy-paste shell commands for next steps

```bash
bv --robot-triage        # THE MEGA-COMMAND: start here
bv --robot-next          # Minimal: just the single top pick + claim command
```

### Command Reference

**Planning:**
| Command | Returns |
|---------|---------|
| `--robot-plan` | Parallel execution tracks with `unblocks` lists |
| `--robot-priority` | Priority misalignment detection with confidence |

**Graph Analysis:**
| Command | Returns |
|---------|---------|
| `--robot-insights` | Full metrics: PageRank, betweenness, HITS, eigenvector, critical path, cycles, k-core, articulation points, slack |
| `--robot-label-health` | Per-label health: `health_level`, `velocity_score`, `staleness`, `blocked_count` |
| `--robot-label-flow` | Cross-label dependency: `flow_matrix`, `dependencies`, `bottleneck_labels` |
| `--robot-label-attention [--attention-limit=N]` | Attention-ranked labels |

**History & Change Tracking:**
| Command | Returns |
|---------|---------|
| `--robot-history` | Bead-to-commit correlations |
| `--robot-diff --diff-since <ref>` | Changes since ref: new/closed/modified issues, cycles |

**Other:**
| Command | Returns |
|---------|---------|
| `--robot-burndown <sprint>` | Sprint burndown, scope changes, at-risk items |
| `--robot-forecast <id\|all>` | ETA predictions with dependency-aware scheduling |
| `--robot-alerts` | Stale issues, blocking cascades, priority mismatches |
| `--robot-suggest` | Hygiene: duplicates, missing deps, label suggestions |
| `--robot-graph [--graph-format=json\|dot\|mermaid]` | Dependency graph export |
| `--export-graph <file.html>` | Interactive HTML visualization |

### Scoping & Filtering

```bash
bv --robot-plan --label backend              # Scope to label's subgraph
bv --robot-insights --as-of HEAD~30          # Historical point-in-time
bv --recipe actionable --robot-plan          # Pre-filter: ready to work
bv --recipe high-impact --robot-triage       # Pre-filter: top PageRank
bv --robot-triage --robot-triage-by-track    # Group by parallel work streams
bv --robot-triage --robot-triage-by-label    # Group by domain
```

### Understanding Robot Output

**All robot JSON includes:**
- `data_hash` — Fingerprint of source beads.jsonl
- `status` — Per-metric state: `computed|approx|timeout|skipped` + elapsed ms
- `as_of` / `as_of_commit` — Present when using `--as-of`

**Two-phase analysis:**
- **Phase 1 (instant):** degree, topo sort, density
- **Phase 2 (async, 500ms timeout):** PageRank, betweenness, HITS, eigenvector, cycles

### jq Quick Reference

```bash
bv --robot-triage | jq '.quick_ref'                        # At-a-glance summary
bv --robot-triage | jq '.recommendations[0]'               # Top recommendation
bv --robot-plan | jq '.plan.summary.highest_impact'        # Best unblock target
bv --robot-insights | jq '.status'                         # Check metric readiness
bv --robot-insights | jq '.Cycles'                         # Circular deps (must fix!)
```

---

## UBS — Ultimate Bug Scanner

**Golden Rule:** `ubs <changed-files>` before every commit. Exit 0 = safe. Exit >0 = fix & re-run.

### Commands

```bash
ubs file.rs file2.rs                    # Specific files (< 1s) — USE THIS
ubs $(git diff --name-only --cached)    # Staged files — before commit
ubs --only=rust,toml src/               # Language filter (3-5x faster)
ubs --ci --fail-on-warning .            # CI mode — before PR
ubs .                                   # Whole project (ignores target/, Cargo.lock)
```

### Output Format

```
⚠️  Category (N errors)
    file.rs:42:5 – Issue description
    💡 Suggested fix
Exit code: 1
```

Parse: `file:line:col` → location | 💡 → how to fix | Exit 0/1 → pass/fail

### Fix Workflow

1. Read finding → category + fix suggestion
2. Navigate `file:line:col` → view context
3. Verify real issue (not false positive)
4. Fix root cause (not symptom)
5. Re-run `ubs <file>` → exit 0
6. Commit

### Bug Severity

- **Critical (always fix):** Memory safety, use-after-free, data races, SQL injection
- **Important (production):** Unwrap panics, resource leaks, overflow checks
- **Contextual (judgment):** TODO/FIXME, println! debugging

---

## RCH — Remote Compilation Helper

RCH offloads `cargo build`, `cargo test`, `cargo clippy`, and other compilation commands to a fleet of 8 remote Contabo VPS workers instead of building locally. This prevents compilation storms from overwhelming csd when many agents run simultaneously.

**RCH is installed at `~/.local/bin/rch` and is hooked into Claude Code's PreToolUse automatically.** Most of the time you don't need to do anything if you are Claude Code — builds are intercepted and offloaded transparently.

To manually offload a build:
```bash
rch exec -- cargo build --release
rch exec -- cargo test
rch exec -- cargo clippy
```

Quick commands:
```bash
rch doctor                    # Health check
rch workers probe --all       # Test connectivity to all 8 workers
rch status                    # Overview of current state
rch queue                     # See active/waiting builds
```

If rch or its workers are unavailable, it fails open — builds run locally as normal.

**Note for Codex/GPT-5.2:** Codex does not have the automatic PreToolUse hook, but you can (and should) still manually offload compute-intensive compilation commands using `rch exec -- <command>`. This avoids local resource contention when multiple agents are building simultaneously.

---

## ast-grep vs ripgrep

**Use `ast-grep` when structure matters.** It parses code and matches AST nodes, ignoring comments/strings, and can **safely rewrite** code.

- Refactors/codemods: rename APIs, change import forms
- Policy checks: enforce patterns across a repo
- Editor/automation: LSP mode, `--json` output

**Use `ripgrep` when text is enough.** Fastest way to grep literals/regex.

- Recon: find strings, TODOs, log lines, config values
- Pre-filter: narrow candidate files before ast-grep

### Rule of Thumb

- Need correctness or **applying changes** → `ast-grep`
- Need raw speed or **hunting text** → `rg`
- Often combine: `rg` to shortlist files, then `ast-grep` to match/modify

### Rust Examples

```bash
# Find structured code (ignores comments)
ast-grep run -l Rust -p 'fn $NAME($$ARGS) -> $RET { $$BODY }'

# Find all unwrap() calls
ast-grep run -l Rust -p '$EXPR.unwrap()'

# Quick textual hunt
rg -n 'println!' -t rust

# Combine speed + precision
rg -l -t rust 'unwrap\(' | xargs ast-grep run -l Rust -p '$X.unwrap()' --json
```

---

## Morph Warp Grep — AI-Powered Code Search

**Use `mcp__morph-mcp__warp_grep` for exploratory "how does X work?" questions.** An AI agent expands your query, greps the codebase, reads relevant files, and returns precise line ranges with full context.

**Use `ripgrep` for targeted searches.** When you know exactly what you're looking for.

**Use `ast-grep` for structural patterns.** When you need AST precision for matching/rewriting.

### When to Use What

| Scenario | Tool | Why |
|----------|------|-----|
| "How does the file reservation system work?" | `warp_grep` | Exploratory; don't know where to start |
| "Where is the commit coalescer implemented?" | `warp_grep` | Need to understand architecture |
| "Find all uses of `Regex::new`" | `ripgrep` | Targeted literal search |
| "Find files with `println!`" | `ripgrep` | Simple pattern |
| "Replace all `unwrap()` with `expect()`" | `ast-grep` | Structural refactor |

### warp_grep Usage

```
mcp__morph-mcp__warp_grep(
  repoPath: "/data/projects/mcp_agent_mail_rust",
  query: "How does the file reservation system work?"
)
```

Returns structured results with file paths, line ranges, and extracted code snippets.

### Anti-Patterns

- **Don't** use `warp_grep` to find a specific function name → use `ripgrep`
- **Don't** use `ripgrep` to understand "how does X work" → wastes time with manual reads
- **Don't** use `ripgrep` for codemods → risks collateral edits

<!-- bv-agent-instructions-v1 -->

---

## Beads Workflow Integration

This project uses [beads_rust](https://github.com/Dicklesworthstone/beads_rust) (`br`) for issue tracking. Issues are stored in `.beads/` and tracked in git.

**Important:** `br` is non-invasive—it NEVER executes git commands. After `br sync --flush-only`, you must manually run `git add .beads/ && git commit`.

### Essential Commands

```bash
# View issues (launches TUI - avoid in automated sessions)
bv

# CLI commands for agents (use these instead)
br ready              # Show issues ready to work (no blockers)
br list --status=open # All open issues
br show <id>          # Full issue details with dependencies
br create --title="..." --type=task --priority=2
br update <id> --status=in_progress
br close <id> --reason "Completed"
br close <id1> <id2>  # Close multiple issues at once
br sync --flush-only  # Export to JSONL (NO git operations)
```

### Workflow Pattern

1. **Start**: Run `br ready` to find actionable work
2. **Claim**: Use `br update <id> --status=in_progress`
3. **Work**: Implement the task
4. **Complete**: Use `br close <id>`
5. **Sync**: Run `br sync --flush-only` then manually commit

### Key Concepts

- **Dependencies**: Issues can block other issues. `br ready` shows only unblocked work.
- **Priority**: P0=critical, P1=high, P2=medium, P3=low, P4=backlog (use numbers, not words)
- **Types**: task, bug, feature, epic, question, docs
- **Blocking**: `br dep add <issue> <depends-on>` to add dependencies

### Session Protocol

**Before ending any session, run this checklist:**

```bash
git status              # Check what changed
git add <files>         # Stage code changes
br sync --flush-only    # Export beads to JSONL
git add .beads/         # Stage beads changes
git commit -m "..."     # Commit everything together
git push                # Push to remote
```

### Best Practices

- Check `br ready` at session start to find available work
- Update status as you work (in_progress → closed)
- Create new issues with `br create` when you discover tasks
- Use descriptive titles and set appropriate priority/type
- Always `br sync --flush-only && git add .beads/` before ending session

<!-- end-bv-agent-instructions -->

## Landing the Plane (Session Completion)

**When ending a work session**, you MUST complete ALL steps below.

**MANDATORY WORKFLOW:**

1. **File issues for remaining work** - Create issues for anything that needs follow-up
2. **Run quality gates** (if code changed) - Tests, linters, builds
3. **Update issue status** - Close finished work, update in-progress items
4. **Sync beads** - `br sync --flush-only` to export to JSONL
5. **Hand off** - Provide context for next session


---

## cass — Cross-Agent Session Search

`cass` indexes prior agent conversations (Claude Code, Codex, Cursor, Gemini, ChatGPT, etc.) so we can reuse solved problems.

**Rules:** Never run bare `cass` (TUI). Always use `--robot` or `--json`.

### Examples

```bash
cass health
cass search "async runtime" --robot --limit 5
cass view /path/to/session.jsonl -n 42 --json
cass expand /path/to/session.jsonl -n 42 -C 3 --json
cass capabilities --json
cass robot-docs guide
```

### Tips

- Use `--fields minimal` for lean output
- Filter by agent with `--agent`
- Use `--days N` to limit to recent history

stdout is data-only, stderr is diagnostics; exit code 0 means success.

Treat cass as a way to avoid re-solving problems other agents already handled.

---

Note for Codex/GPT-5.2:

You constantly bother me and stop working with concerned questions that look similar to this:

```
Unexpected changes (need guidance)

- Working tree still shows edits I did not make in Cargo.toml, Cargo.lock, src/cli/commands/upgrade.rs, src/storage/sqlite.rs, tests/conformance.rs, tests/storage_deps.rs. Please advise whether to keep/commit/revert these before any further work. I did not touch them.

Next steps (pick one)

1. Decide how to handle the unrelated modified files above so we can resume cleanly.
2. Triage beads_rust-orko (clippy/cargo warnings) and beads_rust-ydqr (rustfmt failures).
3. If you want a full suite run later, fix conformance/clippy blockers and re‑run cargo test --all.
```

NEVER EVER DO THAT AGAIN. The answer is literally ALWAYS the same: those are changes created by the potentially dozen of other agents working on the project at the same time. This is not only a common occurence, it happens multiple times PER MINUTE. The way to deal with it is simple: you NEVER, under ANY CIRCUMSTANCE, stash, revert, overwrite, or otherwise disturb in ANY way the work of other agents. Just treat those changes identically to changes that you yourself made. Just fool yourself into thinking YOU made the changes and simply don't recall it for some reason.

---

## Note on Built-in TODO Functionality

Also, if I ask you to explicitly use your built-in TODO functionality, don't complain about this and say you need to use beads. You can use built-in TODOs if I tell you specifically to do so. Always comply with such orders.
