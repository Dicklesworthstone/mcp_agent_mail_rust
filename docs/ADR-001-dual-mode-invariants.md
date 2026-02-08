# ADR-001: Dual-Mode Interface Invariants (MCP Default, CLI Opt-In)

**Status:** Accepted
**Date:** 2026-02-08
**Authors:** WildGate (claude-code/opus-4.6)
**Bead:** br-21gj.1.1

## Context

MCP Agent Mail serves two distinct audiences:

1. **Coding agents** — communicate via the Model Context Protocol (MCP) over
   stdio or HTTP transport. These callers never see a terminal, never parse
   `--help`, and expect JSON-RPC responses.

2. **Human operators** — run administrative commands (`share`, `guard`,
   `archive`, `doctor`, etc.) from a shell. They expect POSIX-style CLI
   ergonomics: `--flags`, exit codes, `stderr` for errors, `stdout` for data.

The current codebase has *two separate binaries* — `mcp-agent-mail` (MCP
server) and `mcp-agent-mail-cli` (operator CLI) — plus the `scripts/am`
launcher. This separation already partially enforces the dual-mode boundary,
but the lack of an explicit, documented contract creates three risks:

- Accidental CLI invocation in MCP mode (e.g., running `mcp-agent-mail
  share export` instead of `mcp-agent-mail-cli share export`).
- Ambiguous behavior when new commands are added — should they go in the
  server binary or the CLI binary?
- Confusion in docs, agents, and CI about which binary to invoke.

## Decision

We establish the following invariants.

### Invariant 1: MCP mode is the default

The `mcp-agent-mail` binary starts in **MCP stdio mode** when invoked with
no subcommand. This is the primary interface for agent integration and must
never change.

```
mcp-agent-mail              → MCP stdio (default)
mcp-agent-mail serve        → MCP HTTP (opt-in)
mcp-agent-mail serve --path /api/  → HTTP with custom path
```

### Invariant 2: CLI mode is a separate binary

Operator commands live exclusively in the `mcp-agent-mail-cli` crate. This
binary is **never invoked by MCP clients**. It connects to the same database
and archive but does not start an MCP transport.

```
mcp-agent-mail-cli share export    → CLI command
mcp-agent-mail-cli guard install   → CLI command
mcp-agent-mail-cli doctor          → CLI command
```

### Invariant 3: No silent mode switching

There is no `--cli-mode`, `INTERFACE_MODE`, or runtime flag that changes the
MCP server binary into a CLI tool. The mode boundary is enforced at the
binary level (compile-time separation), not at runtime.

**Rationale:** Runtime mode switching creates a combinatorial explosion of
test states and makes it trivial for agents to accidentally invoke CLI
commands through the MCP binary.

### Invariant 4: MCP binary rejects unknown subcommands deterministically

If the MCP binary receives a subcommand it does not recognize (e.g.,
`mcp-agent-mail share`), it must:

1. Print a clear error to `stderr`:
   ```
   Error: "share" is not an MCP server command.

   Agent Mail MCP server supports: serve, config
   For operator CLI commands, use: mcp-agent-mail-cli share
   ```
2. Exit with code **2** (usage error, distinct from 1 = runtime error).
3. Never emit JSON-RPC responses for invalid subcommands.

### Invariant 5: `scripts/am` wraps the MCP binary for HTTP mode only

The `am` launcher is a convenience wrapper for `mcp-agent-mail serve`. It
must never route to the CLI crate. Operator tasks should use
`mcp-agent-mail-cli` directly.

### Invariant 6: Mode controls scope, not capability

"Mode" determines which **interface surface** is exposed (MCP tools/resources
vs POSIX CLI commands). Both modes share the same underlying storage, query,
and archive layer. Mode does not gate features — only the presentation layer
differs.

| Aspect | MCP Mode | CLI Mode |
|--------|----------|----------|
| Binary | `mcp-agent-mail` | `mcp-agent-mail-cli` |
| Transport | stdio / HTTP | direct function calls |
| Input format | JSON-RPC | POSIX flags + args |
| Output format | JSON-RPC responses | stdout/stderr text |
| Exit semantics | Process stays alive | Exit code per command |
| Help text | MCP `tools/list` | `--help` flags |
| Error reporting | `McpError` | stderr + exit code |

## Non-Goals

1. **No auto-switching.** The system must never detect "am I being called
   from a terminal?" and silently switch modes. TTY detection is used *only*
   for TUI auto-enable within HTTP serve mode.

2. **No mode fallback.** If MCP mode cannot start (e.g., database
   unreachable), it fails with an error — it does not fall back to CLI mode.

3. **No shared subcommand namespace.** The MCP binary's subcommands (`serve`,
   `config`) and the CLI binary's subcommands (`share`, `guard`, `doctor`,
   etc.) are disjoint sets. No command appears in both binaries.

4. **No runtime mode environment variable.** There is no `AGENT_MAIL_MODE`
   or similar variable. Mode is determined by which binary is executed.

## Consequences

### Positive

- **Clear ownership:** Every new command goes in exactly one binary.
- **Testable:** Each binary can be tested independently with focused
  integration suites.
- **Agent-safe:** Agents cannot accidentally trigger CLI commands through
  the MCP transport.
- **Docs clarity:** README and operator guide can cleanly separate concerns.

### Negative

- **Two binaries to build.** CI must compile and test both. (Mitigated:
  they share workspace crates, so incremental build cost is minimal.)
- **No single entry point.** Operators must know to use
  `mcp-agent-mail-cli` for admin tasks. (Mitigated: the `am` wrapper and
  clear error messages in the MCP binary guide users.)

## Implementation Notes

Downstream beads should reference this ADR when implementing:

- **br-21gj.1.2**: The denial UX contract (Invariant 4 exit codes and
  messages).
- **br-21gj.1.3**: The meta-command allowlist (Invariants 1 & 5 define
  which subcommands the MCP binary accepts).
- **br-21gj.1.4**: The parity target matrix (Invariant 6 defines the
  shared capability layer).
- **br-21gj.2.1**: Config switch (Invariant 3 says no runtime mode flag;
  this bead should implement binary-level separation only).
