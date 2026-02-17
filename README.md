# MCP Agent Mail (Rust)

<div align="center">

[![CI](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/actions/workflows/ci.yml/badge.svg)](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](https://opensource.org/licenses/MIT)

</div>

> "It's like Gmail for your coding agents!"

A mail-like coordination layer for AI coding agents, exposed as an MCP server with 34 tools and 20+ resources, Git-backed archive, SQLite indexing, and an interactive 11-screen TUI operations console. The Rust rewrite of the [original Python project](https://github.com/Dicklesworthstone/mcp_agent_mail) (1,700+ stars).

**Supported agents:** [Claude Code](https://claude.ai/code), [Codex CLI](https://github.com/openai/codex), [Gemini CLI](https://github.com/google-gemini/gemini-cli), [GitHub Copilot CLI](https://docs.github.com/en/copilot), and any MCP-compatible client.

<div align="center">
<h3>Quick Install</h3>

```bash
curl -fsSL "https://raw.githubusercontent.com/Dicklesworthstone/mcp_agent_mail_rust/main/install.sh?$(date +%s)" | bash
```

<p><em>Works on Linux and macOS (x86_64 and aarch64). Auto-detects your platform and downloads the right binary.</em></p>
</div>

---

## TL;DR

**The Problem**: Modern projects often run multiple coding agents at once (backend, frontend, scripts, infra). Without a shared coordination fabric, agents overwrite each other's edits, miss critical context from parallel workstreams, and require humans to relay messages across tools and teams.

**The Solution**: Agent Mail gives every coding agent a persistent identity (e.g., `GreenCastle`), an inbox/outbox, searchable threaded conversations, and advisory file reservations (leases) to signal editing intent. Everything is backed by Git for human-auditable artifacts and SQLite for fast indexing and search.

### Why Use Agent Mail?

| Feature | What It Does |
|---------|--------------|
| **Advisory File Reservations** | Agents declare exclusive or shared leases on file globs before editing, preventing conflicts with a pre-commit guard |
| **Asynchronous Messaging** | Threaded inbox/outbox with subjects, CC/BCC, acknowledgments, and importance levels |
| **Token-Efficient** | Messages stored in a per-project archive, not in agent context windows |
| **34 MCP Tools** | Infrastructure, identity, messaging, contacts, reservations, search, macros, product bus, and build slots |
| **11-Screen TUI** | Real-time dashboard, message browser, thread view, agent roster, search, reservations, metrics, health, timeline, projects, and contacts |
| **Robot Mode** | 16 agent-optimized CLI subcommands with `toon`/`json`/`md` output for non-interactive workflows |
| **Git-Backed Archive** | Every message, reservation, and agent profile stored as files in per-project Git repos |
| **Hybrid Search** | FTS5 full-text search with optional semantic search via frankensearch |
| **Pre-Commit Guard** | Git hook that blocks commits touching files reserved by other agents |
| **Dual-Mode Interface** | MCP server (`mcp-agent-mail`) and operator CLI (`am`) share tools but enforce strict surface separation |

### Quick Example

```bash
# Install and start (auto-detects all installed coding agents)
am

# That's it. Server starts on 127.0.0.1:8765 with the interactive TUI.

# Agents coordinate through MCP tools:
#   ensure_project(project_key="/abs/path")
#   register_agent(project_key, program="claude-code", model="opus-4.6")
#   file_reservation_paths(project_key, agent_name, ["src/**"], ttl_seconds=3600, exclusive=true)
#   send_message(project_key, from_agent, to_agent, subject="Starting refactor", thread_id="FEAT-123")
#   fetch_inbox(project_key, agent_name)

# Or use the robot CLI for non-interactive agent workflows:
am robot status --project /abs/path --agent BlueLake
am robot inbox --project /abs/path --agent BlueLake --urgent --format json
am robot reservations --project /abs/path --agent BlueLake --conflicts
```

---

## Why This Exists

One disciplined hour of AI coding agents often produces 10-20 "human hours" of work because the agents reason and type at machine speed. Agent Mail multiplies that advantage by keeping independent agents aligned without babysitting:

- **Prevents conflicts:** Explicit file reservations (leases) for files/globs prevent agents from overwriting each other
- **Eliminates human liaisons:** Agents send messages directly to each other with threaded conversations, acknowledgments, and priority levels
- **Keeps communication off the token budget:** Messages stored in per-project Git archive, not consuming agent context windows
- **Offers quick reads:** `resource://inbox/{Agent}`, `resource://thread/{id}`, and 20+ other MCP resources
- **Provides full audit trails:** Every instruction, lease, message, and attachment is in Git for human review
- **Scales across repos:** Frontend and backend agents in different repos coordinate through the product bus and contact system

### Typical Use Cases

- Multiple agents splitting a large refactor across services while staying in sync
- Frontend and backend agent teams coordinating thread-by-thread across repositories
- Protecting critical migrations with exclusive file reservations and pre-commit guards
- Searching and summarizing long technical discussions as threads evolve
- Running agent swarms with [Beads](https://github.com/Dicklesworthstone/beads_rust) task tracking for dependency-aware work selection

---

## Design Philosophy

**Mail metaphor, not chat.** Agents send discrete messages with subjects, recipients, and thread IDs. This maps cleanly to how work coordination actually happens: structured communication with clear intent, not a firehose of chat messages.

**Git as the source of truth.** Every message, agent profile, and reservation artifact lives as a file in a per-project Git repository. This makes the entire communication history human-auditable, diffable, and recoverable. SQLite is the fast index, not the authority.

**Advisory, not mandatory.** File reservations are advisory leases, not hard locks. The pre-commit guard enforces them at commit time, but agents can always override with `--no-verify` if needed. This prevents deadlocks while still catching accidental conflicts.

**Dual persistence.** Human-readable Markdown in Git for auditability; SQLite with FTS5 for sub-millisecond queries, search, and directory lookups. Both stay in sync through the write pipeline.

**Structured concurrency, no Tokio.** The entire async stack uses [asupersync](https://github.com/Dicklesworthstone/asupersync) with `Cx`-threaded structured concurrency. No orphan tasks, cancel-correct channels, and deterministic testing with virtual time.

---

## Installation

### One-Liner (recommended)

```bash
curl -fsSL "https://raw.githubusercontent.com/Dicklesworthstone/mcp_agent_mail_rust/main/install.sh?$(date +%s)" | bash
```

Downloads the right binary for your platform, installs to `~/.local/bin`, and optionally updates your `PATH`. Supports `--verify` for checksum + Sigstore cosign verification.

Options: `--version vX.Y.Z`, `--dest DIR`, `--system` (installs to `/usr/local/bin`), `--from-source`, `--verify`, `--easy-mode` (auto-update PATH), `--force`.

### From Source

```bash
git clone https://github.com/Dicklesworthstone/mcp_agent_mail_rust
cd mcp_agent_mail_rust
cargo build --release
# Binaries at target/release/mcp-agent-mail and target/release/am
```

Requires Rust nightly (see `rust-toolchain.toml`). Also requires sibling workspace crates: `fastmcp_rust`, `sqlmodel_rust`, `asupersync`, `frankentui`, `beads_rust`, `frankensearch`, `toon_rust`.

### Platforms

| Platform | Architecture | Binary |
|----------|-------------|--------|
| Linux | x86_64 | `mcp-agent-mail-x86_64-unknown-linux-gnu` |
| Linux | aarch64 | `mcp-agent-mail-aarch64-unknown-linux-gnu` |
| macOS | x86_64 | `mcp-agent-mail-x86_64-apple-darwin` |
| macOS | Apple Silicon | `mcp-agent-mail-aarch64-apple-darwin` |

---

## Quick Start

### 1. Start the server

```bash
am
```

Auto-detects all installed coding agents (Claude Code, Codex CLI, Gemini CLI, etc.), configures their MCP connections, and starts the HTTP server on `127.0.0.1:8765` with the interactive TUI.

### 2. Agents register and coordinate

Once the server is running, agents use MCP tools to coordinate:

```
# Register identity
ensure_project(project_key="/abs/path/to/repo")
register_agent(project_key, program="claude-code", model="opus-4.6")

# Reserve files before editing
file_reservation_paths(project_key, agent_name, ["src/**"], ttl_seconds=3600, exclusive=true)

# Send a message
send_message(project_key, from_agent="GreenCastle", to_agent="BlueLake",
             subject="Starting auth refactor", thread_id="FEAT-123", ack_required=true)

# Check inbox
fetch_inbox(project_key, agent_name)
acknowledge_message(project_key, agent_name, message_id)
```

### 3. Use macros for common flows

```
# Boot a full session (register + discover inbox + check reservations)
macro_start_session(project_key, program, model)

# Prepare for a thread (fetch context + recent messages)
macro_prepare_thread(project_key, agent_name, thread_id)

# Reserve, work, release cycle
macro_file_reservation_cycle(project_key, agent_name, paths, ttl_seconds)

# Contact handshake between agents in different projects
macro_contact_handshake(from_project, from_agent, to_project, to_agent)
```

---

## Server Modes

### MCP Server (default)

```bash
mcp-agent-mail                          # stdio transport (for MCP client integration)
mcp-agent-mail serve                    # HTTP server with TUI (default 127.0.0.1:8765)
mcp-agent-mail serve --no-tui           # Headless server (CI/daemon mode)
mcp-agent-mail serve --reuse-running    # Reuse existing server on same port
```

### CLI Operator Tool

```bash
am                                      # Auto-detect agents, configure MCP, start server + TUI
am serve-http --port 9000               # Different port
am serve-http --host 0.0.0.0            # Bind to all interfaces
am serve-http --no-auth                 # Skip authentication (local dev)
am serve-http --path api                # Use /api/ transport instead of /mcp/
am --help                               # Full operator CLI
```

### Dual-Mode Interface

This project keeps MCP server and CLI command surfaces separate:

| Use case | Entry point | Notes |
|---------|-------------|-------|
| MCP server (default) | `mcp-agent-mail` | Default: MCP stdio transport. HTTP: `serve`. |
| CLI (operator + agent-first) | `am` | Recommended CLI entry point. |
| CLI via single binary | `AM_INTERFACE_MODE=cli mcp-agent-mail` | Same CLI surface, one binary. |

Running CLI-only commands via the MCP binary produces a deterministic denial on stderr with exit code `2`, and vice versa. This prevents accidental mode confusion in automated workflows.

---

## The 34 MCP Tools

### 9 Clusters

| Cluster | Count | Tools |
|---------|-------|-------|
| Infrastructure | 4 | `health_check`, `ensure_project`, `ensure_product`, `products_link` |
| Identity | 3 | `register_agent`, `create_agent_identity`, `whois` |
| Messaging | 5 | `send_message`, `reply_message`, `fetch_inbox`, `acknowledge_message`, `mark_message_read` |
| Contacts | 4 | `request_contact`, `respond_contact`, `list_contacts`, `set_contact_policy` |
| File Reservations | 4 | `file_reservation_paths`, `renew_file_reservations`, `release_file_reservations`, `force_release_file_reservation` |
| Search | 2 | `search_messages`, `summarize_thread` |
| Macros | 4 | `macro_start_session`, `macro_prepare_thread`, `macro_contact_handshake`, `macro_file_reservation_cycle` |
| Product Bus | 5 | `ensure_product`, `products_link`, `search_messages_product`, `fetch_inbox_product`, `summarize_thread_product` |
| Build Slots | 3 | `acquire_build_slot`, `renew_build_slot`, `release_build_slot` |

### 20+ MCP Resources

Read-only resources for fast lookups without tool calls:

```
resource://inbox/{Agent}?project=<abs-path>&limit=20
resource://thread/{id}?project=<abs-path>&include_bodies=true
resource://agents?project=<abs-path>
resource://reservations?project=<abs-path>
resource://metrics
resource://health
```

### Macros vs. Granular Tools

- **Prefer macros when you want speed** or are on a smaller model: `macro_start_session`, `macro_prepare_thread`, `macro_file_reservation_cycle`, `macro_contact_handshake`
- **Use granular tools when you need control:** `register_agent`, `file_reservation_paths`, `send_message`, `fetch_inbox`, `acknowledge_message`

---

## TUI Operations Console

The interactive TUI has 11 screens. Number keys `1`-`9`, `0` for screen 10; `Tab` or `Ctrl+P` to reach any screen:

| # | Screen | Shows |
|---|--------|-------|
| 1 | Dashboard | Real-time event stream with sparkline and Braille heatmap |
| 2 | Messages | Message browser with search and filtering |
| 3 | Threads | Thread view with correlation |
| 4 | Agents | Registered agents with activity indicators |
| 5 | Search | Query bar + facets + results + preview |
| 6 | Reservations | File reservations with TTL countdowns |
| 7 | Tool Metrics | Per-tool latency and call counts |
| 8 | System Health | Connection probes, disk/memory, circuit breakers |
| 9 | Timeline | Chronological event timeline with inspector |
| 10 | Projects | Project list and routing helpers |
| 11 | Contacts | Contact graph and policy surface |

**Key bindings:** `?` help, `Ctrl+P` command palette, `m` toggle MCP/API, `Shift+T` cycle theme, `q` quit, vim-style visual multi-selection with batch actions.

**Themes:** Cyberpunk Aurora, Darcula, Lumen Light, Nordic Frost, High Contrast. Accessibility support with high-contrast mode and reduced motion.

---

## Robot Mode (`am robot`)

Non-interactive, agent-first CLI surface for TUI-equivalent situational awareness. Use it when you need structured snapshots quickly, especially in automated loops and when tokens matter.

### 16 Subcommands

| Command | Purpose | Key flags |
|---------|---------|-----------|
| `am robot status` | Dashboard synthesis | `--format`, `--project`, `--agent` |
| `am robot inbox` | Actionable inbox with urgency/ack synthesis | `--urgent`, `--ack-overdue`, `--unread`, `--all`, `--limit`, `--include-bodies` |
| `am robot timeline` | Event stream since last check | `--since`, `--kind`, `--source` |
| `am robot overview` | Cross-project summary | `--format`, `--project`, `--agent` |
| `am robot thread <id>` | Full thread rendering | `--limit`, `--since`, `--format` |
| `am robot search <query>` | Full-text search with facets/relevance | `--kind`, `--importance`, `--since`, `--format` |
| `am robot message <id>` | Single-message deep view | `--format`, `--project`, `--agent` |
| `am robot navigate <resource://...>` | Resolve resources into robot-formatted output | `--format`, `--project`, `--agent` |
| `am robot reservations` | Reservation view with conflict/expiry awareness | `--all`, `--conflicts`, `--expiring`, `--agent` |
| `am robot metrics` | Tool call rates, failures, latency percentiles | `--format`, `--project`, `--agent` |
| `am robot health` | Runtime/system diagnostics | `--format`, `--project`, `--agent` |
| `am robot analytics` | Anomaly and remediation summary | `--format`, `--project`, `--agent` |
| `am robot agents` | Agent roster and activity overview | `--active`, `--sort` |
| `am robot contacts` | Contact graph and policy surface | `--format`, `--project`, `--agent` |
| `am robot projects` | Per-project aggregate stats | `--format`, `--project`, `--agent` |
| `am robot attachments` | Attachment inventory and provenance | `--format`, `--project`, `--agent` |

### Output Formats

- **`toon`** (default at TTY): Token-efficient, compact, optimized for agent parsing
- **`json`** (default when piped): Machine-readable envelope with `_meta`, `_alerts`, `_actions`
- **`md`** (thread/message-focused): Human-readable narrative for deep context

### Agent Workflow Recipes

```bash
# Startup triage
am robot status --project /abs/path --agent AgentName

# Immediate urgency pass
am robot inbox --project /abs/path --agent AgentName --urgent --format json

# Incremental monitoring loop
am robot timeline --project /abs/path --agent AgentName --since 2026-02-16T10:00:00Z

# Deep thread drill-down
am robot thread br-123 --project /abs/path --agent AgentName --format md

# Reservation safety check before edits
am robot reservations --project /abs/path --agent AgentName --conflicts --expiring 30
```

---

## File Reservations for Multi-Agent Editing

Before editing, agents reserve file paths to avoid conflicts:

```
file_reservation_paths(project_key, agent_name, ["src/**"], ttl_seconds=3600, exclusive=true)
```

The pre-commit guard (`mcp-agent-mail-guard`) installs as a Git hook and blocks commits that touch files reserved by other agents. Reservations are advisory, TTL-based, and support glob patterns.

| Area | Reserve glob |
|------|-------------|
| Core types/config | `crates/mcp-agent-mail-core/src/**` |
| SQLite layer | `crates/mcp-agent-mail-db/src/**` |
| Git archive | `crates/mcp-agent-mail-storage/src/**` |
| Tool implementations | `crates/mcp-agent-mail-tools/src/**` |
| TUI | `crates/mcp-agent-mail-server/src/tui_*.rs` |
| CLI/launcher | `crates/mcp-agent-mail-cli/src/**` |

---

## Multi-Agent Coordination Workflows

### Same Repository

1. **Register identity:** `ensure_project` + `register_agent` using the repo's absolute path as `project_key`
2. **Reserve files before editing:** `file_reservation_paths(project_key, agent_name, ["src/**"], ttl_seconds=3600, exclusive=true)`
3. **Communicate with threads:** `send_message(..., thread_id="FEAT-123")`, check with `fetch_inbox`, acknowledge with `acknowledge_message`
4. **Quick reads:** `resource://inbox/{Agent}?project=<abs-path>&limit=20`

### Across Different Repos

- **Option A (single project bus):** Register both repos under the same `project_key`. Keep reservation patterns specific (`frontend/**` vs `backend/**`).
- **Option B (separate projects):** Each repo has its own `project_key`. Use `macro_contact_handshake` to link agents, then message directly. Keep a shared `thread_id` across repos.

### With Beads Task Tracking

Agent Mail pairs with [Beads](https://github.com/Dicklesworthstone/beads_rust) (`br`) for dependency-aware task selection:

1. **Pick ready work:** `br ready --json` (choose highest priority, no blockers)
2. **Reserve edit surface:** `file_reservation_paths(..., reason="br-123")`
3. **Announce start:** `send_message(..., thread_id="br-123", subject="[br-123] Start: <title>", ack_required=true)`
4. **Work and update:** Reply in-thread with progress
5. **Complete and release:** `br close 123`, `release_file_reservations(...)`, final Mail reply

Use the Beads issue ID (`br-123`) as the Mail `thread_id` and prefix message subjects with `[br-123]` to keep everything linked.

---

## Browser State Sync Endpoint

For browser/WASM clients, Agent Mail exposes a polling-based state sync contract:

- `GET /mail/ws-state` returns a snapshot payload
- `GET /mail/ws-state?since=<seq>&limit=<n>` returns deltas since a sequence
- `POST /mail/ws-input` accepts remote terminal ingress events (`Input`, `Resize`)

Note: `/mail/ws-state` is intentionally HTTP polling, not WebSocket upgrade. WebSocket upgrade attempts return `501 Not Implemented`.

```bash
# Snapshot
curl -sS 'http://127.0.0.1:8765/mail/ws-state?limit=50' | jq .

# Delta from a known sequence
curl -sS 'http://127.0.0.1:8765/mail/ws-state?since=1200&limit=200' | jq .

# Input ingress (key event)
curl -sS -X POST 'http://127.0.0.1:8765/mail/ws-input' \
  -H 'Content-Type: application/json' \
  --data '{"type":"Input","data":{"kind":"Key","key":"j","modifiers":0}}' | jq .
```

---

## Deployment Validation

```bash
# Export a bundle
am share export -o /tmp/agent-mail-bundle --no-zip

# Verify a live deployment against the bundle
am share deploy verify-live https://example.github.io/agent-mail \
  --bundle /tmp/agent-mail-bundle \
  --json > /tmp/verify-live.json

# Inspect verdict
jq '.verdict, .summary, .config' /tmp/verify-live.json
```

Exit codes: `0` = pass, `1` = fail.

---

## Configuration

All configuration via environment variables. The server reads them at startup via `Config::from_env()`.

| Variable | Default | Description |
|----------|---------|-------------|
| `AM_INTERFACE_MODE` | (unset = MCP) | `mcp` or `cli` |
| `HTTP_HOST` | `127.0.0.1` | Bind address |
| `HTTP_PORT` | `8765` | Bind port |
| `HTTP_PATH` | `/mcp/` | MCP base path |
| `HTTP_BEARER_TOKEN` | (from `.env` file) | Auth token |
| `DATABASE_URL` | `sqlite:///:memory:` | SQLite connection URL |
| `STORAGE_ROOT` | `~/.mcp_agent_mail` | Archive root directory |
| `LOG_LEVEL` | `info` | Minimum log level |
| `TUI_ENABLED` | `true` | Interactive TUI toggle |
| `TUI_HIGH_CONTRAST` | `false` | Accessibility mode |
| `WORKTREES_ENABLED` | `false` | Build slots feature flag |

For the full list of 100+ env vars, see `crates/mcp-agent-mail-core/src/config.rs`.

For operations guidance and troubleshooting, see [docs/OPERATOR_RUNBOOK.md](docs/OPERATOR_RUNBOOK.md).

---

## Architecture

Cargo workspace with strict dependency layering:

```text
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
├── Cargo.toml                              # Workspace root (12 member crates)
├── crates/
│   ├── mcp-agent-mail-core/                # Zero-dep: config, models, errors, metrics
│   ├── mcp-agent-mail-db/                  # SQLite schema, queries, pool, cache, FTS, hybrid search
│   ├── mcp-agent-mail-storage/             # Git archive, commit coalescer, notification signals
│   ├── mcp-agent-mail-search-core/         # Pluggable search traits
│   ├── mcp-agent-mail-guard/               # Pre-commit guard, reservation enforcement
│   ├── mcp-agent-mail-share/               # Snapshot, scrub, bundle, crypto, export
│   ├── mcp-agent-mail-tools/               # 34 MCP tool implementations (9 clusters)
│   ├── mcp-agent-mail-server/              # HTTP/MCP runtime, dispatch, TUI (11 screens)
│   ├── mcp-agent-mail/                     # Server binary (mcp-agent-mail)
│   ├── mcp-agent-mail-cli/                 # CLI binary (am) with robot mode
│   ├── mcp-agent-mail-conformance/         # Python parity tests
│   └── mcp-agent-mail-wasm/                # Browser/WASM TUI frontend
├── tests/e2e/                              # End-to-end test scripts
├── scripts/                                # CLI integration tests, utilities
├── docs/                                   # ADRs, specs, runbooks, migration guides
├── install.sh                              # Multi-platform installer
└── rust-toolchain.toml                     # Nightly toolchain requirement
```

### Storage Layout

```
~/.mcp_agent_mail/                          # STORAGE_ROOT
├── projects/
│   └── {project_slug}/
│       ├── .git/                           # Per-project git repository
│       ├── messages/
│       │   └── {YYYY}/{MM}/               # Date-partitioned canonical messages
│       ├── agents/
│       │   └── {agent_name}/
│       │       ├── inbox/                  # Agent inbox copies
│       │       └── outbox/                 # Agent outbox copies
│       ├── build_slots/                    # Build slot leases (JSON)
│       └── file_reservations/              # Reservation artifacts
└── .archive.lock                           # Global advisory lock
```

### Key Design Decisions

- **Git-backed archive** for human auditability; SQLite as fast index
- **WAL mode** with PRAGMA tuning and connection pooling for concurrent access
- **Write-behind cache** with dual-indexed ReadCache and deferred touch batching (30s flush)
- **Async git commit coalescer** (write-behind queue) to avoid commit storms
- **i64 microseconds** for all timestamps (no `chrono::NaiveDateTime` in storage layer)
- **FTS5 full-text search** with sanitized queries and LIKE fallback
- **Conformance testing** against the Python reference implementation (23 tools, 23+ resources)
- **Advisory file reservations** with symmetric fnmatch, archive reading, and rename handling
- **`#![forbid(unsafe_code)]`** across all crates
- **asupersync exclusively** for all async operations (no Tokio, reqwest, hyper, or axum)

---

## Comparison vs. Alternatives

| Feature | MCP Agent Mail | Shared Files / Lockfiles | Custom MCP Tools | Chat-Based Coordination |
|---------|---------------|-------------------------|------------------|------------------------|
| Agent identity & discovery | Persistent names, profiles, directory queries | None | Manual | Ephemeral |
| Threaded messaging | Subjects, CC/BCC, ack, importance, search | N/A | Build yourself | Linear chat, no structure |
| File reservation / advisory locks | Glob patterns, TTL, exclusive/shared, pre-commit guard | Lockfiles (no TTL, no glob) | Build yourself | None |
| Audit trail | Full Git history of all communication | File timestamps | Depends | Chat logs (if saved) |
| Cross-repo coordination | Product bus, contact system | Manual | Build yourself | Manual |
| Search | FTS5 + optional semantic search | `grep` | Build yourself | Limited |
| Operational visibility | 11-screen TUI, robot CLI, metrics | None | None | None |
| Token efficiency | Messages stored externally, not in context | Files in context | Varies | All in context |

---

## Development

```bash
# Quality gates
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo check --workspace --all-targets
cargo test --workspace

# Conformance tests (parity with Python reference)
cargo test -p mcp-agent-mail-conformance

# Run specific crate tests
cargo test -p mcp-agent-mail-db
cargo test -p mcp-agent-mail-tools
cargo test -p mcp-agent-mail-server

# E2E tests
tests/e2e/test_stdio.sh         # MCP stdio transport
tests/e2e/test_http.sh           # HTTP transport
tests/e2e/test_guard.sh          # Pre-commit guard
tests/e2e/test_macros.sh         # Macro tools
tests/e2e/test_share.sh          # Share/export
tests/e2e/test_dual_mode.sh      # Mode switching
scripts/e2e_cli.sh               # CLI integration (99 assertions)

# Benchmarks
cargo bench -p mcp-agent-mail

# Multi-agent builds: isolate target dir to avoid lock contention
export CARGO_TARGET_DIR="/tmp/target-$(whoami)-am"
```

### Key Dependencies

| Crate | Purpose |
|-------|---------|
| `asupersync` | Structured async runtime (channels, sync, regions, HTTP, testing) |
| `fastmcp_rust` | MCP protocol implementation (JSON-RPC, stdio, HTTP transport) |
| `sqlmodel_rust` | SQLite ORM (schema, queries, migrations, pool) |
| `frankentui` | TUI rendering (widgets, themes, accessibility, markdown, syntax highlighting) |
| `frankensearch` | Hybrid search engine (lexical + semantic, two-tier fusion, reranking) |
| `beads_rust` | Issue tracking integration |
| `toon` | Token-efficient compact encoding for robot mode output |

---

## Troubleshooting

| Problem | Fix |
|---------|-----|
| `"from_agent not registered"` | Always `register_agent` in the correct `project_key` first |
| `"FILE_RESERVATION_CONFLICT"` | Adjust patterns, wait for TTL expiry, or use non-exclusive reservation |
| Auth errors with JWT | Include bearer token with matching `kid` in the request header |
| Port 8765 already in use | `am serve-http --port 9000` or stop the existing server |
| TUI not rendering | Check `TUI_ENABLED=true` and that your terminal supports 256 colors |
| Empty inbox | Verify recipient names match exactly and messages were sent to that agent |
| Search returns nothing | Try simpler terms; FTS5 falls back to LIKE for edge cases |
| Pre-commit guard blocking | Check `am robot reservations --conflicts` for active reservations |

---

## Limitations

- **Rust nightly required.** Uses Rust 2024 edition features that require the nightly compiler.
- **Local workspace dependencies.** Building from source requires sibling directories for `fastmcp_rust`, `sqlmodel_rust`, `asupersync`, `frankentui`, `frankensearch`, `beads_rust`, and `toon_rust`.
- **Single-machine coordination.** Designed for agents running on the same machine or accessing the same filesystem. Not a distributed system.
- **Advisory, not enforced.** File reservations are advisory. Agents can bypass the pre-commit guard with `--no-verify`.
- **No built-in authentication federation.** JWT support exists, but there's no centralized auth service. Each server manages its own tokens.

---

## FAQ

**Q: How is this different from the Python version?**
A: This is a ground-up Rust rewrite with the same conceptual model but significant improvements: an 11-screen interactive TUI, robot mode CLI, hybrid search, build slots, the product bus for cross-project coordination, and substantially better performance. The conformance test suite ensures output format parity with the Python reference.

**Q: Do I need to run a separate server for each project?**
A: No. One server handles multiple projects. Each project is identified by its absolute filesystem path as the `project_key`.

**Q: How do agents get their names?**
A: Agent Mail generates memorable adjective+noun names (e.g., `GreenCastle`, `BlueLake`, `RedHarbor`) when agents register. Agents can also specify a name explicitly.

**Q: Can agents in different repos talk to each other?**
A: Yes. Use `request_contact` / `respond_contact` (or `macro_contact_handshake`) to establish a link between agents in different projects, then message directly. The product bus enables cross-project search and inbox queries.

**Q: Does this work with Claude Code's Max subscription?**
A: Yes. You can use a Max account with Agent Mail. Each agent session connects to the same MCP server regardless of subscription tier.

**Q: What happens if the server crashes?**
A: Git is the source of truth. SQLite indexes can be rebuilt from the Git archive. The commit coalescer uses a write-behind queue that flushes on graceful shutdown.

**Q: How does this integrate with Beads?**
A: Use the Beads issue ID (e.g., `br-123`) as the Mail `thread_id`. Beads owns task status/priority/dependencies; Agent Mail carries the conversations and audit trail. The installer can optionally set up Beads alongside Agent Mail.

---

## Ready-Made Blurb for Your AGENTS.md

Add this to your project's `AGENTS.md` or `CLAUDE.md` to help agents use Agent Mail effectively:

```
## MCP Agent Mail: coordination for multi-agent workflows

What it is
- A mail-like layer that lets coding agents coordinate asynchronously via MCP tools and resources.
- Provides identities, inbox/outbox, searchable threads, and advisory file reservations,
  with human-auditable artifacts in Git.

Why it's useful
- Prevents agents from stepping on each other with explicit file reservations (leases).
- Keeps communication out of your token budget by storing messages in a per-project archive.
- Offers quick reads (resource://inbox/..., resource://thread/...) and macros that bundle common flows.

How to use effectively
1) Register an identity: call ensure_project, then register_agent using this repo's
   absolute path as project_key.
2) Reserve files before you edit: file_reservation_paths(project_key, agent_name,
   ["src/**"], ttl_seconds=3600, exclusive=true)
3) Communicate with threads: use send_message(..., thread_id="FEAT-123"); check inbox
   with fetch_inbox and acknowledge with acknowledge_message.
4) Quick reads: resource://inbox/{Agent}?project=<abs-path>&limit=20

Macros vs granular tools
- Prefer macros for speed: macro_start_session, macro_prepare_thread,
  macro_file_reservation_cycle, macro_contact_handshake.
- Use granular tools for control: register_agent, file_reservation_paths, send_message,
  fetch_inbox, acknowledge_message.

Common pitfalls
- "from_agent not registered": always register_agent in the correct project_key first.
- "FILE_RESERVATION_CONFLICT": adjust patterns, wait for expiry, or use non-exclusive.
```

---

## Documentation

| Document | Purpose |
|----------|---------|
| [OPERATOR_RUNBOOK.md](docs/OPERATOR_RUNBOOK.md) | Deployment, troubleshooting, diagnostics |
| [DEVELOPER_GUIDE.md](docs/DEVELOPER_GUIDE.md) | Dev setup, debugging, testing |
| [MIGRATION_GUIDE.md](docs/MIGRATION_GUIDE.md) | Python to Rust migration |
| [RELEASE_CHECKLIST.md](docs/RELEASE_CHECKLIST.md) | Pre-release validation |
| [ROLLOUT_PLAYBOOK.md](docs/ROLLOUT_PLAYBOOK.md) | Staged rollout strategy |
| [ADR-001](docs/ADR-001-dual-mode-invariants.md) | Dual-mode interface design |
| [ADR-002](docs/ADR-002-single-binary-cli-opt-in.md) | Single binary with mode switch |
| [ADR-003](docs/ADR-003-search-v3-architecture.md) | Search v3 implementation |

---

## About Contributions

Please don't take this the wrong way, but I do not accept outside contributions for any of my projects. I simply don't have the mental bandwidth to review anything, and it's my name on the thing, so I'm responsible for any problems it causes; thus, the risk-reward is highly asymmetric from my perspective. I'd also have to worry about other "stakeholders," which seems unwise for tools I mostly make for myself for free. Feel free to submit issues, and even PRs if you want to illustrate a proposed fix, but know I won't merge them directly. Instead, I'll have Claude or Codex review submissions via `gh` and independently decide whether and how to address them. Bug reports in particular are welcome. Sorry if this offends, but I want to avoid wasted time and hurt feelings. I understand this isn't in sync with the prevailing open-source ethos that seeks community contributions, but it's the only way I can move at this velocity and keep my sanity.

---

## License

MIT
