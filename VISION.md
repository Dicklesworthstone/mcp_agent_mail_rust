# MCP Agent Mail (Rust) — Unified Product Vision

> Synthesized from 10 days of user direction (Feb 4-14, 2026).
> Later statements override earlier ones where they conflict.

---

## The One-Sentence Vision

**Typing `am` in an interactive terminal opens the operations console and starts or attaches to the local MCP server. Setup discovers coding agents and configures their connections during installation or on explicit request.**

---

## Core Philosophy: "Total Auto / Just Works"

These principles are non-negotiable and override all other design considerations:

1. **Automatic discovery** — Discover supported clients, machine settings, and mailbox state wherever their authority is clear. Register individual agent identities through MCP or the CLI.
2. **Predictable startup** — Bare `am` keeps the existing client configuration and uses the configured endpoint. When stdout is non-interactive it reports robot status.
3. **Native operator interface** — The `am` binary owns setup, runtime, migration, and diagnostics commands. The distribution installers authenticate and install the released binary pair.
4. **Showcase Quality** — The TUI must match the frankentui demo showcase in every way: visual polish, rounded borders, focus-aware panels, gradient text, rich charts, mouse support, semantic color tokens. "Same level of quality and attention to detail."
5. **Zero Garbling** — ANSI escape codes must never appear as visible characters. Emoji widths must "just work" automatically without library users thinking about it.

---

## Interface Architecture

### What Happens When You Type `am`

Interactive `am` uses the loopback HTTP endpoint `127.0.0.1:8765` by default. It can attach to an existing Agent Mail endpoint. An unrelated listener is a startup error. Use `am serve-http --no-tui` for an intentional headless launch and `am serve-stdio` for stdio transport. See the [operator cookbook](docs/OPERATOR_COOKBOOK.md).

### Two Audiences, One Binary

| Audience | Interface | Design Goal |
|----------|-----------|-------------|
| **Human operators** | Full-screen TUI | Showcase-demo-quality visual experience with real-time dashboards, search, mouse interaction |
| **Coding agents** | `am robot` CLI + MCP tools | Hyper-optimized, token-efficient JSON/TOON/Markdown output; the interface YOU would want if YOU were using it |

---

## TUI: Showcase-Quality Operations Console

The TUI must be visually indistinguishable in quality from the frankentui demo showcase. Every screen must use frankentui's rich widget library natively — no ANSI string building, no manual cell rendering, no Paragraph widgets where Table/Sparkline/MiniBar should be used.

### 16 Registered Screens

| # | Screen | What It Shows | Key Widgets |
|---|--------|---------------|-------------|
| 1 | **Dashboard** | Real-time event stream, throughput sparklines, Braille heatmap, DB stats as MiniBar gauges | Sparkline, MiniBar, LogViewer, Panel (ROUNDED) |
| 2 | **Messages** | Message browser with **search-as-you-type** across all mail content, inline body preview snippets | Table (sortable/filterable), TextInput, Panel |
| 3 | **Threads** | Thread view with message correlation, participant list, timeline | Tree, Table, Timeline |
| 4 | **Agents** | Registered agents with activity sparklines, capability badges, last-seen indicators | Table (sortable), Sparkline per row, StatusDot |
| 5 | **Search** | **Global search-as-you-type** with facets (project, agent, date range) + live result preview. Modeled on frankentui Shakespeare/SQLite search examples | TextInput, List, Panel (split preview), Facet chips |
| 6 | **Reservations** | File reservations with TTL countdown progress bars, path grouping, conflict highlighting | Table, ProgressBar per row, Tree (path hierarchy) |
| 7 | **Tool Metrics** | Per-tool latency sparklines, call/error counts, MiniBar error rate gauges | Table, Sparkline, MiniBar |
| 8 | **System Health** | Connection probes, disk/memory usage, circuit breaker states, pool utilization | MiniBar gauges, StatusDot, Panel grid |
| 9 | **Timeline** | Chronological event stream with inspector panel, zoom, correlation lines | LogViewer, Panel (split detail) |
| 10 | **Projects** | Project list, routing, sibling relationships | Table, Tree |
| 11 | **Contacts** | Contact graph, policy surface, approval queue | Table, StatusDot, Panel |
| 12 | **Explorer** | Unified inbox/outbox explorer with direction and acknowledgement filters | Table, Panel, Filter chips |
| 13 | **Analytics** | Anomaly insight feed with confidence scoring and deep links | Table, Sparkline, Panel |
| 14 | **Attachments** | Attachment inventory with preview and provenance metadata | Table, Preview panel, StatusDot |
| 15 | **Archive Browser** | Two-pane Git archive browser with tree navigation and file preview | Tree, Panel, Preview pane |
| 16 | **ATC** | Snapshot-driven Air Traffic Controller surface with decision drill-in and retention visibility | Table, Panel, Status cards |

### Visual Requirements (Match Showcase Demo)

- **Borders**: ROUNDED by default (`╭╮╰╯`), HEAVY for emphasis, DOUBLE for critical alerts
- **Focus states**: Active panel has bright colored border + title highlight; inactive panels have dim borders
- **Color system**: All colors via semantic `TuiThemePalette` tokens — zero hardcoded `Color::Rgb(...)` anywhere
- **Theme cycling**: `Shift+T` rotates through Dark/Light/Solarized/Nord (minimum 4 themes)
- **Mouse support**: Click to focus panels, click table rows to select, scroll wheel in all scrollable areas
- **Keyboard**: `?` help overlay, `Ctrl+P` command palette (25+ actions), `Tab`/`Shift+Tab` panel focus cycling, number keys for screen switching
- **Shortcuts bar**: Always visible at bottom showing available shortcuts for current screen
- **Toasts**: Real-time event notifications via `NotificationQueue` overlay (auto-dismiss, severity-colored)
- **Responsive**: Panels reflow on terminal resize. No garbled layouts at any reasonable terminal size.

### Search-as-You-Type (Critical Feature)

This was requested multiple times and must be implemented exactly like the frankentui showcase search examples:

1. **Keystroke-driven**: Every character typed immediately filters/searches results. No "press Enter to search."
2. **Global scope**: Searches across message subjects, bodies, agent names, thread IDs — everything.
3. **Live preview**: Selected result shows full message body in adjacent preview panel.
4. **Faceted filtering**: Filter by project, agent, date range, importance level.
5. **Two-tier semantic search**: Fast potion-128M pass gives instant results; background MiniLM-L6 pass upgrades rankings with smooth visual re-ordering as better results arrive.
6. **Debounce**: Short debounce (50-100ms) to avoid hammering SQLite on every keystroke.

---

## Agent "Robot Mode" CLI

For coding agents that interact with `am` programmatically. The interface an agent would design for itself.

```bash
# Inbox (default: JSON, compact)
am robot inbox --agent BlueLake --limit 10 --format json
am robot inbox --agent BlueLake --format toon    # Token-efficient toon format
am robot inbox --agent BlueLake --format md       # Markdown for context injection

# Search
am robot search "deployment failed" --project /backend --format json

# Status (everything an agent needs to orient itself)
am robot status --project /backend --format json
# Returns: active agents, unread count, open reservations, recent activity

# Reservations
am robot reserve --paths "src/**" --ttl 3600 --exclusive
am robot release --paths "src/**"

# Thread operations
am robot thread br-123 --include-bodies --format md

# All am functionality accessible via robot subcommands
# Output always structured (never interactive, never TUI)
# Default format: JSON. Also supports: toon, md, yaml
```

---

## Agent Discovery and MCP Configuration

`am setup status` shows discovered clients and their configuration. Use `am setup run --dry-run` to preview the writes, `am setup run` to apply them, or `am serve-http --setup` to apply them for the requested server endpoint. Native setup uses `franken-agent-detection` and the configured home directories. Environment overrides and client-specific locations are covered in the [operator cookbook](docs/OPERATOR_COOKBOOK.md).

Setup merges Agent Mail entries with existing client configuration. It keeps the bearer token in the private canonical config, normally `${XDG_CONFIG_HOME:-$HOME/.config}/mcp-agent-mail/config.env`. Agent registration and transport policy remain explicit. Any move toward automatic client rewrites at bare startup would require deciding when those writes have operator authorization.

---

## Performance Architecture

### SQLite Hardening (Implemented)
- WAL mode, PRAGMA tuning, connection pooling
- Write-behind cache with dual indexing and deferred touch batching (30s flush)
- Async git commit coalescer (batches writes to avoid commit storms)

### frankensqlite Integration (Implemented, Hardening Ongoing)

**Current state.** Live mailbox queries use `DbConn` through `sqlmodel-frankensqlite`. The binaries also statically bundle C SQLite through `sqlmodel-sqlite` for independent checks in doctor, reconstruction, and legacy import (`CanonicalDbConn`). `FSQLITE_CONCURRENT_MODE` remains opt-in while MVCC hardening continues.

**Next hardening work:**
- Keep closing any remaining engine feature gaps directly in FrankenSQLite rather than reintroducing C SQLite
- Replace all `BEGIN IMMEDIATE` with `BEGIN CONCURRENT` for page-level MVCC (128 concurrent writers)
- MVCC conflict detection already wired (`is_mvcc_conflict()` in error.rs)

**Non-negotiable:** Never reintroduce a production/runtime dependency on C SQLite. The direction is forward — toward full FrankenSQLite ownership and hardening.

### asupersync Everywhere

The workspace uses Asupersync for async work and HTTP. Its structured-concurrency context flows from callers. The [workspace rules](AGENTS.md) describe the dependency boundary and production feature checks.

---

## Tool Documentation Preservation

> "The documentation for each MCP tool, the way that incorrect tool calls are handled, etc. from the original Python project are MASTERPIECES OF ENGINEERING AND THE RESULT OF MONTHS OF WORK AND RESEARCH. EVERY SINGLE ELEMENT OF THEM — LITERALLY TO THE LAST PUNCTUATION MARKS — MUST BE PRESERVED EXACTLY IN THIS RUST VERSION."

This is absolute. Tool descriptions, parameter names, error messages, help text — byte-for-byte parity with Python. Conformance tests enforce this.

> **Reality note (2026-09-01):** what the conformance suite actually enforces today is the tool inventory (38 compatibility + 7 Rust-native), parameter names and input-schema compatibility (required arguments may relax but never tighten), error-code envelopes, and resource coverage. Tool prose is Rust-owned: `crates/mcp-agent-mail-conformance/tests/tool_description_parity.rs` compares descriptions to the Python fixture but records a mismatch as `PASS (Rust wording revised)`. Byte-for-byte prose parity is therefore a policy statement, not a gate.

---

## Console Output Quality

### Startup Banner
Rich ASCII art "MCP MAIL" logo in bright cyan, server config table (ROUNDED borders, bright blue), DB stats table, success message. Full parity with Python's `rich_logger.py:display_startup_banner()`.

### Tool Call Logging
Structured panels for each tool invocation showing: tool name, parameters (with sensitive values masked as `●●●●●●●●`), duration, result summary. Not raw JSON dumps.

### ANSI Garbling Fix (Critical)
The staircase garbling bug was caused by pushing ANSI-escaped strings into `LogPane` via `Text::raw()`, which treated escape codes as visible characters. The fix is an `ansi_to_text()` converter that properly parses ANSI SGR sequences into styled `ftui::text::Text` objects. This must be bulletproof — zero garbling under any circumstances.

### Emoji Width Handling
VS16 (U+FE0F) variation selectors must be stripped before width calculation in `grapheme_width()`. Text-default emoji with VS16 render as 1 cell in most terminals but `unicode-display-width` reports 2. The fix is framework-level in `ftui-core` so all users benefit automatically.

---

## Testing Philosophy

- **Real tests, no mocks** — All tests use real SQLite databases, real git repos, real file I/O
- **Conformance testing** against Python fixtures — 37 tools, 25 resources, byte-level output comparison
- **E2E test scripts** with detailed logging across all transports (stdio, HTTP, CLI)
- **Stress tests** — Concurrent operations, pool exhaustion recovery, cache coherency under load
- **Property tests** — Fuzzing for edge cases in parsing, validation, serialization
- **Every test must pass** — Zero known failures, zero flaky tests, zero ignored tests

---

## What's NOT Part of This Vision

- Web UI (deferred — focus is terminal-native TUI)
- WASM/JS frankentui mode (mentioned but deprioritized vs core TUI quality)
- Multi-machine federation (single-machine, multi-agent coordination only)
- Backwards compatibility shims (no users yet, do it right with zero tech debt)

---

## Priority Stack (What to Fix/Build First)

1. **Reliable `am` startup** — Keep interactive launch, live attach, explicit setup, and headless operation reliable across supported platforms.
2. **Fix ANSI garbling** — Zero garbled output anywhere in the TUI. Ever.
3. **Search-as-you-type** — The most-requested missing feature. Must work like frankentui showcase search.
4. **Visual polish to showcase quality** — Theme system, semantic colors, focus states, rounded borders, mouse support.
5. **All 16 screens fully implemented** — No placeholder stubs. Every screen uses real widgets with real data.
6. **Robot mode CLI** — Agent-optimized `am robot` subcommands with JSON/TOON/Markdown output.
7. **FrankenSQLite hardening** — Keep the live path on FrankenSQLite and qualify opt-in concurrent writes under realistic workloads.

---

*This document represents the canonical, synthesized product vision. When in doubt about what to build or how, refer here. Later user statements always override earlier ones.*
