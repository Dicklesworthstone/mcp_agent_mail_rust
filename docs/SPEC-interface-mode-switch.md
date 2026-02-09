# Spec: Runtime Interface Mode Switch (MCP Default, CLI Opt-In)

**Bead:** br-163x.2  
**Depends on:** ADR-002 (br-163x.1)  
**Date:** 2026-02-09

## Scope

Defines:

- The **single** runtime setting that selects which interface surface the `mcp-agent-mail` binary
  exposes.
- Precedence rules and normalization.
- Help/usage behavior per mode.
- Wrong-mode denial UX (MCP→CLI and CLI→MCP) and exit codes.
- Test vectors for unit/integration/E2E coverage.

Non-goals:

- Heuristic/implicit switching (TTY detection, “looks like a CLI command”, fallback).
- Supporting multiple simultaneous interfaces within one invocation.

## Setting

### `AM_INTERFACE_MODE`

`AM_INTERFACE_MODE` controls the interface surface exposed by the `mcp-agent-mail` binary.

Allowed values (case-insensitive, surrounding whitespace ignored):

| Value | Meaning |
|------|---------|
| (unset) | Default: MCP mode |
| `mcp` | MCP mode |
| `cli` | CLI mode |

Normalization:

- Empty string is treated as unset (default MCP).
- Any other value is an error (usage error).

Rationale:

- Env-only keeps MCP-mode `--help` clean (no mode flags advertised by default).
- The default remains safe for agent integrations (no silent changes).

## Precedence

This spec defines **env-only** mode selection:

1. `AM_INTERFACE_MODE` (process env) is authoritative.
2. No config-file setting and no CLI flag participates in mode selection.

If a future CLI flag is introduced, it must be explicitly specified here and must not violate the
help-surface invariants below.

## Mode Semantics

### MCP Mode (default)

When `AM_INTERFACE_MODE` is unset or set to `mcp`:

- `mcp-agent-mail` with no args starts MCP stdio mode.
- `mcp-agent-mail serve` starts HTTP mode.
- `mcp-agent-mail config` prints config.
- Any other top-level subcommand is treated as a **CLI-only attempt** and must be rejected with the
  MCP-mode denial UX (below).

### CLI Mode

When `AM_INTERFACE_MODE=cli`:

- The `mcp-agent-mail` binary exposes the **full CLI surface** (equivalent to `am`).
- `mcp-agent-mail --help` renders CLI help, ideally using `mcp-agent-mail` in usage strings.
- `mcp-agent-mail` with no args renders CLI help (or a deterministic short usage) and exits with a
  usage error code per clap defaulting rules (see exit codes).

### Help Surface Invariants

- MCP mode help must not list CLI-only subcommands.
- CLI mode help must list CLI subcommands (and must not pretend the binary is `am` unless we cannot
  practically override the clap app name).

## Wrong-Mode Denial UX

### MCP mode: CLI attempt denial

Trigger: `AM_INTERFACE_MODE` is unset/`mcp` and argv[1] is not one of the MCP allowlisted
subcommands.

Requirements:

- Print to **stderr** only.
- Exit code `2`.
- Include:
  - quoted attempted subcommand (argv[1])
  - allowed MCP commands list
  - remediation path(s):
    - use the `am` binary (or `mcp-agent-mail-cli`)
    - OR set `AM_INTERFACE_MODE=cli` and rerun via `mcp-agent-mail`

Template (exact phrasing can be tuned, but must remain deterministic):

```
Error: "{command}" is not an MCP server command.

Agent Mail MCP server accepts: serve, config
For operator CLI commands, use: am {command}
Or enable CLI mode: AM_INTERFACE_MODE=cli mcp-agent-mail {command} ...
```

### CLI mode: MCP-only attempt denial

Trigger: `AM_INTERFACE_MODE=cli` and argv[1] is an MCP-only command that is not part of the CLI
surface (notably `serve` and `config`, unless explicitly added to the CLI surface).

Requirements:

- Print to **stderr** only.
- Exit code `2`.
- Include:
  - current mode (`AM_INTERFACE_MODE=cli`)
  - remediation:
    - unset `AM_INTERFACE_MODE` (or set `mcp`) and rerun `mcp-agent-mail serve ...`
    - OR use the CLI equivalents if they exist (e.g. `serve-http`, `serve-stdio`)

Template:

```
Error: "{command}" is not available in CLI mode (AM_INTERFACE_MODE=cli).

To start the MCP server:
  unset AM_INTERFACE_MODE   # (or set AM_INTERFACE_MODE=mcp)
  mcp-agent-mail serve ...
```

## Exit Codes

| Code | Meaning | Examples |
|------|---------|----------|
| 0 | success | normal operation |
| 1 | runtime error | server startup failure, IO error |
| 2 | usage error / wrong-mode | unknown subcommand, invalid mode value, wrong-mode denial |

## Test Vectors (Must Be Covered)

Unit / integration:

1. MCP default: `mcp-agent-mail share export` → exit 2, stderr contains canonical denial, stdout empty.
2. CLI mode: `AM_INTERFACE_MODE=cli mcp-agent-mail --help` → exit 0, output is CLI help surface.
3. CLI mode wrong-mode: `AM_INTERFACE_MODE=cli mcp-agent-mail serve` → exit 2 with deterministic denial.
4. Invalid value: `AM_INTERFACE_MODE=wat mcp-agent-mail --help` → exit 2 with deterministic error.

E2E:

- Add matrix rows mirroring the vectors above with per-row artifacts (stdout/stderr/exit + command
  line) under `tests/artifacts/<suite>/<timestamp>/`.

