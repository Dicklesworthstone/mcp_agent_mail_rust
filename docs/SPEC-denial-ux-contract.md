# Spec: MCP-Mode CLI Denial UX Contract and Exit-Code Policy

**Bead:** br-21gj.1.2
**Depends on:** ADR-001 (br-21gj.1.1) (superseded), ADR-002 (br-163x.1)
**Date:** 2026-02-08
**Status:** Amended by `docs/SPEC-interface-mode-switch.md` (2026-02-09)

## Scope

Defines the exact behavior when an operator invokes the MCP server binary
(`mcp-agent-mail`) with an unrecognized subcommand (i.e., a CLI-only
command).

## Canonical Denial Message

When the MCP binary receives a subcommand outside its allowlist (see
SPEC-meta-command-allowlist.md), it prints the following to **stderr**:

```
Error: "{command}" is not an MCP server command.

Agent Mail is not a CLI.
Agent Mail MCP server accepts: serve, config
For operator CLI commands, use: am {command}
Or enable CLI mode: AM_INTERFACE_MODE=cli mcp-agent-mail {command} ...

Tip: Run `am --help` for the full command list.
```

### Template Variables

| Placeholder | Source |
|------------|--------|
| `{command}` | The first positional argument (subcommand name) |

### Format Requirements

1. First line: `Error: ` prefix + quoted command + explanation.
2. Blank line separator.
3. Remediation block: deterministic lines showing what *is* accepted and how to fix.
4. Optional tip line.

## Exit Code Policy

| Code | Meaning | When |
|------|---------|------|
| 0 | Success | Normal MCP operation, `--help`, `--version` |
| 1 | Runtime error | Database unreachable, transport failure, etc. |
| 2 | Usage error | Unknown subcommand, invalid flags, bad arguments |

Exit code **2** is this project's standard wrong-surface and generic usage
code. This allows shell scripts and CI to distinguish "user called the wrong
binary" from "server crashed."

Exception: the `am` CLI binary's retired legacy `serve` subcommand exits `64`
with classifier `legacy-subcommand-migration`. That path is a compatibility
preflight for supervisors that previously retried `am serve`; exit `64` means
"migrate command, do not retry unchanged." The MCP server binary command
`mcp-agent-mail serve` is still valid, and the CLI-mode wrong-surface denial for
`mcp-agent-mail serve` still exits `2`.

## Output Channel Policy

| Channel | Content |
|---------|---------|
| **stderr** | All denial messages, error messages, warnings |
| **stdout** | MCP JSON-RPC only (stdio mode), or nothing (denial) |

The denial path must **never** write to stdout. In stdio mode, stdout is the
MCP transport. Writing non-JSON-RPC content to stdout would corrupt the
protocol stream.

## MCP Tool-Name Correction Block

For known MCP tool names and common shorthands, the server binary must not emit
the misleading generic `am {command}` remediation. It must instead print a
deterministic correction block with the nearest CLI command and, where
applicable, the MCP tool name to call through an MCP client.

Current covered inputs: `reserve`, `file-reserve`, `file_reservation_paths`,
`macro_start_session`, `send_message`, `send`, `inbox`, `fetch_inbox`,
`reservations`, `serve-http`, and `serve-stdio`.
The `serve-http` and `serve-stdio` corrections intentionally omit the
`MCP tool:` line because `serve` is an MCP server command, not a tool call.

## Tone and Verbosity

1. **Terse for agents.** If `NO_COLOR=1` or `stdout` is not a TTY, omit the
   "Tip:" line.
2. **Helpful for humans.** When a TTY is detected, include the full
   remediation block with the tip.
3. **No backtrace.** Denial is not an error condition worthy of a stack
   trace.
4. **No JSON output.** The denial path does not support `--json` or
   `--output json` flags. It is plain text on stderr.

## Snapshot Test Contract

Downstream implementation (br-21gj.3.2) must include snapshot tests that
assert:

1. Exact stderr output for known denied commands (`share`, `guard`,
   `doctor`).
2. Exit code 2.
3. Empty stdout.
4. Behavior with `NO_COLOR=1`.
