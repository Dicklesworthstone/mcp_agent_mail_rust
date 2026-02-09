# ADR-002: Runtime CLI Opt-In for `mcp-agent-mail` (Default MCP)

**Status:** Accepted  
**Date:** 2026-02-09  
**Authors:** RedHarbor (codex/gpt-5)  
**Bead:** br-163x.1  
**Supersedes:** ADR-001 (Invariant 3: “No silent mode switching / no runtime flag”)

## Context

MCP Agent Mail has two high-value interfaces:

1. **MCP server interface** (agents): JSON-RPC over stdio or HTTP.
2. **Operator / agent-first CLI** (humans and automation): POSIX flags, exit codes, stdout/stderr UX.

Today the codebase provides these via **two binaries**:

- `mcp-agent-mail` (server)
- `am` (CLI; built from `mcp-agent-mail-cli`)

This split is workable, but users have requested a **single entrypoint** that can behave like the CLI
when explicitly enabled, while keeping the current MCP defaults intact.

The key risk is accidental breakage of agent integrations: *MCP must remain the default and must not
silently change*.

## Decision

We will support a **runtime opt-in** that makes the `mcp-agent-mail` binary expose the CLI surface.

### Invariant 1: MCP remains the default

If the opt-in setting is absent, `mcp-agent-mail` behaves exactly as it does today:

```
mcp-agent-mail         → MCP stdio (default)
mcp-agent-mail serve   → MCP HTTP (opt-in)
```

### Invariant 2: CLI mode is explicit opt-in (OFF by default)

CLI mode is enabled only when an explicit setting is present:

- `AM_INTERFACE_MODE=cli` → CLI surface (equivalent to `am`)
- `AM_INTERFACE_MODE=mcp` (or unset) → MCP surface

Exact precedence details (and any aliases) are defined in `br-163x.2`.

### Invariant 3: No heuristic switching

There is no TTY-based auto-switch, no “looks like a CLI command” heuristic, and no fallback between
modes. The mode decision is made once at process start and is authoritative.

### Invariant 4: Help surface is mode-correct

- In MCP mode, `mcp-agent-mail --help` must not advertise CLI subcommands.
- In CLI mode, `mcp-agent-mail --help` must render the CLI help surface (ideally using
  `mcp-agent-mail` in usage strings, not `am`).

### Invariant 5: Wrong-mode commands fail deterministically

- MCP mode: CLI attempts (e.g. `mcp-agent-mail share export`) must fail fast with a high-signal
  denial message and exit code `2`.
- CLI mode: MCP-only attempts must fail fast with an equally deterministic message and exit code `2`.

The exact denial phrasing and remediation hints are a spec surface (see `br-163x.2` and `br-163x.5`).

## Consequences

### Positive

- **Single entrypoint option:** users can standardize on `mcp-agent-mail` if desired.
- **Default safety preserved:** agents still get MCP behavior by default.
- **Reduced confusion:** the same binary can be used consistently across environments with an explicit
  switch.

### Negative

- **Expanded test matrix:** behavior must be proven in both modes (unit + integration + E2E).
- **Increased coupling:** `mcp-agent-mail` becomes a dispatcher into the CLI library.

## Implementation Notes

- `mcp-agent-mail` must decide the mode *before* parsing its own clap subcommands, so that `--help`
  and denial UX are correct for the selected surface.
- In CLI mode, dispatch should call into `mcp_agent_mail_cli` (library crate) rather than shelling out
  to `am`.
- The CLI library likely needs a small API to render usage/help under the invoking name
  (`mcp-agent-mail` vs `am`). This is tracked in `br-163x.4`.

