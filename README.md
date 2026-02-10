# MCP Agent Mail (Rust)

Mail-like coordination layer for coding agents. Provides an MCP server with
34 tools and 20+ resources, Git-backed archive, SQLite indexing, and an
interactive TUI operations console.

## Quick Start

```bash
scripts/am
```

That's it. The `am` command starts the HTTP server on `127.0.0.1:8765` with
the interactive TUI. It auto-discovers your auth token from
`~/.mcp_agent_mail/.env` and defaults to the `/mcp/` transport path.

Common variations:

```bash
scripts/am --api              # Use /api/ transport instead of /mcp/
scripts/am --port 9000        # Different port
scripts/am --host 0.0.0.0     # Bind to all interfaces
scripts/am --no-tui           # Headless server (no interactive TUI)
scripts/am --no-auth          # Skip authentication (local dev)
```

### Alternative: Direct Binary

```bash
# HTTP server (same as am, but no convenience defaults)
cargo run -p mcp-agent-mail -- serve --host 127.0.0.1 --port 8765

# stdio transport (for MCP client integration)
cargo run -p mcp-agent-mail

# CLI tool (runs the `am` binary)
cargo run -p mcp-agent-mail-cli -- --help
```

### Dual-Mode Interface (MCP Server vs CLI)

This repo exposes **two command surfaces** from a single `mcp-agent-mail` binary:

| Use case | Entry point | Notes |
|---------|-------------|-------|
| MCP server (default) | `mcp-agent-mail` | Default is MCP stdio transport. HTTP is `serve`. |
| CLI (operator + agent-first) | `am` | Built from the `mcp-agent-mail-cli` crate. |
| CLI via single binary | `AM_INTERFACE_MODE=cli mcp-agent-mail` | Same CLI surface, one binary. |

**Default behavior (MCP mode):** Running CLI-only commands via the MCP binary
produces a deterministic denial on `stderr` with exit code `2`:

```bash
cargo run -p mcp-agent-mail -- share export
# Error: "share" is not an MCP server command.
# Agent Mail MCP server accepts: serve, config
# For operator CLI commands, use: am share
# Or enable CLI mode: AM_INTERFACE_MODE=cli mcp-agent-mail share ...
```

**Optional CLI mode:** Set `AM_INTERFACE_MODE=cli` to use the full CLI
surface through the `mcp-agent-mail` binary:

```bash
AM_INTERFACE_MODE=cli mcp-agent-mail mail send \
  --project /abs/path/to/project \
  --from RedHarbor \
  --to OrangeFinch \
  --subject "hello" \
  --body "test"
```

In CLI mode, MCP-only commands (`serve`, `config`) are denied with an equally
deterministic message pointing back to MCP mode.

The `am` binary remains unchanged and is the recommended CLI entry point.

Note: `scripts/am` is a dev convenience wrapper around `mcp-agent-mail serve`.
It is not the `am` CLI binary.

For the canonical contract/specs:
- `docs/ADR-001-dual-mode-invariants.md`
- `docs/ADR-002-single-binary-cli-opt-in.md`
- `docs/SPEC-interface-mode-switch.md`
- `docs/SPEC-denial-ux-contract.md`
- `docs/SPEC-parity-matrix.md`

## TUI Controls

The interactive TUI has 11 screens. Number keys map to screens 1-10 (`1`-`9`, `0`=10); use `Tab` or `Ctrl+P` to reach any screen:

| # | Screen       | Shows                                              |
|---|--------------|-----------------------------------------------------|
| 1 | Dashboard    | Real-time event stream with sparkline               |
| 2 | Messages     | Message browser with search and filtering            |
| 3 | Threads      | Thread view with correlation                         |
| 4 | Agents       | Registered agents with activity indicators           |
| 5 | Search       | Query bar + facets + results + preview               |
| 6 | Reservations | File reservations with TTL countdowns                |
| 7 | Tool Metrics | Per-tool latency and call counts                     |
| 8 | SystemHealth | Connection probes, disk/memory, circuit breakers     |
| 9 | Timeline     | Chronological event timeline with inspector          |
| 10 | Projects    | Project list and routing helpers                     |
| 11 | Contacts    | Contact graph and policy surface                     |

Key bindings: `?` help, `Ctrl+P` command palette, `m` toggle MCP/API,
`Shift+T` cycle theme, `q` quit.

## Configuration

All configuration is via environment variables. The server reads them at
startup via `Config::from_env()`. Key variables:

| Variable              | Default            | Description               |
|-----------------------|--------------------|---------------------------|
| `AM_INTERFACE_MODE`   | (unset = MCP)      | `mcp` or `cli` (ADR-002)  |
| `HTTP_HOST`           | `127.0.0.1`        | Bind address              |
| `HTTP_PORT`           | `8765`             | Bind port                 |
| `HTTP_PATH`           | `/mcp/`            | MCP base path             |
| `HTTP_BEARER_TOKEN`   | (from `.env` file) | Auth token                |
| `DATABASE_URL`        | `sqlite:///:memory:`| SQLite connection URL    |
| `STORAGE_ROOT`        | `~/.mcp_agent_mail`| Archive root directory    |
| `LOG_LEVEL`           | `info`             | Minimum log level         |
| `TUI_HIGH_CONTRAST`   | `false`            | Accessibility mode        |

For the full list of 100+ env vars, see
`crates/mcp-agent-mail-core/src/config.rs`.

For operations guidance, troubleshooting, and diagnostics, see
[docs/OPERATOR_RUNBOOK.md](docs/OPERATOR_RUNBOOK.md).

## Architecture

Cargo workspace with strict dependency layering:

```text
mcp-agent-mail-core          (config, models, errors, metrics)
  ├─ mcp-agent-mail-db       (SQLite schema, queries, pool)
  ├─ mcp-agent-mail-storage  (Git archive, commit coalescer)
  ├─ mcp-agent-mail-guard    (pre-commit guard, reservation enforcement)
  ├─ mcp-agent-mail-share    (snapshot, bundle, export)
  └─ mcp-agent-mail-tools    (34 MCP tool implementations)
       └─ mcp-agent-mail-server  (HTTP/MCP runtime, TUI, web UI)
            ├─ mcp-agent-mail        (server binary)
            ├─ mcp-agent-mail-cli    (am CLI binary)
            └─ mcp-agent-mail-conformance  (parity tests)
```

### File Reservations for Multi-Agent Editing

Before editing, agents should reserve file paths to avoid conflicts:

| Area               | Reserve glob                                 |
|--------------------|----------------------------------------------|
| Core types/config  | `crates/mcp-agent-mail-core/src/**`          |
| SQLite layer       | `crates/mcp-agent-mail-db/src/**`            |
| Git archive        | `crates/mcp-agent-mail-storage/src/**`       |
| Tool implementations | `crates/mcp-agent-mail-tools/src/**`       |
| TUI                | `crates/mcp-agent-mail-server/src/tui_*.rs`  |
| CLI/launcher       | `crates/mcp-agent-mail-cli/src/**`, `scripts/am` |

## Development

```bash
# Quality gates
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test

# Conformance tests (parity with legacy Python)
cargo test -p mcp-agent-mail-conformance

# Benchmarks
cargo bench -p mcp-agent-mail

# Multi-agent builds: isolate target dir to avoid lock contention
export CARGO_TARGET_DIR="/tmp/target-$(whoami)-am"
```

## Notes

- Rust nightly required (see `rust-toolchain.toml`)
- Uses local crates: `/dp/fastmcp_rust`, `/dp/sqlmodel_rust`,
  `/dp/asupersync`, `/dp/frankentui`, `/dp/beads_rust`,
  `/dp/coding_agent_session_search`
