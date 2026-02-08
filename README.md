# MCP Agent Mail (Rust)

Mail-like coordination layer for coding agents. Provides an MCP server with
23 tools and 20+ resources, Git-backed archive, SQLite indexing, and an
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

# CLI tool
cargo run -p mcp-agent-mail-cli -- --help
```

## TUI Controls

The interactive TUI has 7 screens navigable with `1`-`7` or `Tab`:

| # | Screen       | Shows                                              |
|---|--------------|-----------------------------------------------------|
| 1 | Dashboard    | Real-time event stream with sparkline               |
| 2 | Messages     | Message browser with search and filtering            |
| 3 | Threads      | Thread view with correlation                         |
| 4 | Agents       | Registered agents with activity indicators           |
| 5 | Reservations | File reservations with TTL countdowns                |
| 6 | ToolMetrics  | Per-tool latency and call counts                     |
| 7 | SystemHealth | Connection probes, disk/memory, circuit breakers     |

Key bindings: `?` help, `Ctrl+P` command palette, `m` toggle MCP/API,
`Shift+T` cycle theme, `q` quit.

## Configuration

All configuration is via environment variables. The server reads them at
startup via `Config::from_env()`. Key variables:

| Variable            | Default            | Description               |
|---------------------|--------------------|---------------------------|
| `HTTP_HOST`         | `127.0.0.1`        | Bind address              |
| `HTTP_PORT`         | `8765`             | Bind port                 |
| `HTTP_PATH`         | `/mcp/`            | MCP base path             |
| `HTTP_BEARER_TOKEN` | (from `.env` file) | Auth token                |
| `DATABASE_URL`      | `sqlite:///:memory:`| SQLite connection URL    |
| `STORAGE_ROOT`      | `~/.mcp_agent_mail`| Archive root directory    |
| `LOG_LEVEL`         | `info`             | Minimum log level         |
| `TUI_HIGH_CONTRAST` | `false`            | Accessibility mode        |

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
  └─ mcp-agent-mail-tools    (23 MCP tool implementations)
       └─ mcp-agent-mail-server  (HTTP/MCP runtime, TUI, web UI)
            ├─ mcp-agent-mail        (server binary)
            ├─ mcp-agent-mail-cli    (operator CLI)
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
