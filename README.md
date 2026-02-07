# MCP Agent Mail (Rust Port)

This repository is a Rust port of the legacy Python `mcp_agent_mail` project. It provides a
mail‑like coordination layer for coding agents via an MCP server, with Git‑backed artifacts and
SQLite indexing.

## Quick Start

### Easiest Local Run (`am`)
```bash
cd /data/projects/mcp_agent_mail_rust
alias am='/data/projects/mcp_agent_mail_rust/scripts/am'
am serve
```

What `am serve` does by default:
- sets `HTTP_PATH=/mcp/`
- sets `LOG_RICH_ENABLED=true`
- auto-loads `HTTP_BEARER_TOKEN` from `~/mcp_agent_mail/.env` if not already set

### Run MCP Server (stdio)
```bash
cargo run -p mcp-agent-mail
```

### Run MCP Server (HTTP)
```bash
cargo run -p mcp-agent-mail -- serve --host 127.0.0.1 --port 8765
```

Notes:
- The server now accepts both `/mcp/` and `/api/` as MCP HTTP base-path aliases for local/dev compatibility.
- `scripts/am` options: `--path mcp|api`, `--host`, `--port`, `--env-file`, `--no-auth`.

### Run CLI
```bash
cargo run -p mcp-agent-mail-cli -- --help
```

## Configuration (Env Vars)

Config is loaded from environment variables (and `.env` in the working directory). See
`crates/mcp-agent-mail-core/src/config.rs` for the full set.

Console/logging related:

- `LOG_RICH_ENABLED` (default: `true`): enable rich/ftui console output.
- `LOG_TOOL_CALLS_ENABLED` (default: `true`): enable tool call start/end panel logging.
- `LOG_TOOL_CALLS_RESULT_MAX_CHARS` (default: `2000`): max characters of tool result JSON rendered in panels.

Console layout + persistence (rich TTY only; interactive tuning is HTTP mode only):

- `CONSOLE_PERSIST_PATH` (default: `~/.config/mcp-agent-mail/config.env`): user-local envfile where console prefs are read/written. Read from real env only; `CONSOLE_*` keys do not fall back to repo `.env`.
- `CONSOLE_AUTO_SAVE` (default: `true`): automatically persist console prefs on interactive changes.
- `CONSOLE_INTERACTIVE` (default: `true`): enable interactive console layout tuning (HTTP + TTY only).
- `CONSOLE_UI_HEIGHT_PERCENT` (default: `33`, clamp `10..80`): inline HUD height percent.
- `CONSOLE_UI_ANCHOR` (default: `bottom`): `bottom|top`.
- `CONSOLE_UI_AUTO_SIZE` (default: `false`): use inline auto-sizing mode.
- `CONSOLE_INLINE_AUTO_MIN_ROWS` (default: `8`, clamp `>=4`).
- `CONSOLE_INLINE_AUTO_MAX_ROWS` (default: `18`, clamp `>=min`).
- `CONSOLE_SPLIT_MODE` (default: `inline`): `inline|left` (left requires AltScreen work; see `br-1m6a.20`).
- `CONSOLE_SPLIT_RATIO_PERCENT` (default: `30`, clamp `10..80`): requested left split ratio.
- `CONSOLE_THEME` (default: `cyberpunk_aurora`): console theme id (see `br-1m6a.12`).

## Conformance Tests

Run fixture-based parity tests against the Rust server router:
```bash
cargo test -p mcp-agent-mail-conformance
```

### Regenerate Fixtures (Legacy Python)
Use the legacy Python venv (bundled in `legacy_python_mcp_agent_mail_code/mcp_agent_mail/.venv`):
```bash
legacy_python_mcp_agent_mail_code/mcp_agent_mail/.venv/bin/python \
  crates/mcp-agent-mail-conformance/tests/conformance/python_reference/generate_fixtures.py
```

## Benchmarks
```bash
cargo bench -p mcp-agent-mail
```

## Dev Workflow

Quality gates:
```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

## Multi-Agent Builds (Target Dir Isolation)

When multiple agents build in parallel, set a per-agent target dir to avoid lock contention:
```bash
export CARGO_TARGET_DIR="/tmp/target-$(whoami)-am"
```

## Notes

- Rust nightly is required (see `rust-toolchain.toml`).
- This port uses local crates for MCP, SQLite, async IO, and UI:
  - `/dp/fastmcp_rust`
  - `/dp/sqlmodel_rust`
  - `/dp/asupersync`
  - `/dp/frankentui`
  - `/dp/beads_rust`
  - `/dp/coding_agent_session_search`
