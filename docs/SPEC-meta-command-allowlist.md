# Spec: MCP-Mode Meta-Command Allowlist

**Bead:** br-21gj.1.3
**Depends on:** ADR-001 (br-21gj.1.1)
**Date:** 2026-02-08

## Scope

Even in MCP mode, a small set of meta-commands must remain accessible for
operability. This spec enumerates that allowlist and the rationale for each
entry.

## Allowlist

| Command | Rationale | Output |
|---------|-----------|--------|
| *(no subcommand)* | Default: start MCP stdio transport. Core purpose of the binary. | JSON-RPC on stdout |
| `serve` | Start MCP HTTP transport with optional TUI. Required for all HTTP deployments. | HTTP listener + optional TUI |
| `config` | Show resolved configuration for debugging. Needed before MCP transport starts. | Text to stdout |
| `--version` | Standard convention. CI/CD and operators need version introspection. | Version string to stdout |
| `--help` / `-h` | Standard convention. Necessary for discoverability. Must list allowed commands only. | Help text to stdout |

**Total: 3 subcommands + 2 flags.**

## Security and Usability Tradeoffs

### Why not more?

Every additional command in the MCP binary:

- Increases the attack surface for accidental CLI execution in agent
  workflows.
- Adds maintenance burden (must be tested in both binary contexts).
- Dilutes the "MCP-first" identity of the binary.

### Why not fewer?

Removing `config` would make it impossible to debug configuration issues
without starting the MCP transport. Since `config` is read-only and
side-effect-free, the risk is minimal.

### What about `doctor`?

`doctor` performs active I/O (probe database, check git, verify archive).
It belongs in the CLI binary where its output format and error handling
can be operator-oriented. Operators who need diagnostics can run
`mcp-agent-mail-cli doctor check`.

### What about `migrate`?

`migrate` modifies the database schema. It is a destructive operation that
must be run intentionally by an operator, never by an MCP client. It stays
in the CLI binary.

## Guard Implementation Contract

The centralized CLI gate (br-21gj.3.1) must:

1. Match the incoming subcommand against this allowlist.
2. If matched: route to the handler.
3. If not matched: invoke the denial renderer (br-21gj.3.2).
4. The allowlist must be a `const` array in the source, not a runtime config
   value, to prevent accidental widening.

```rust
/// Commands accepted by the MCP server binary.
const MCP_ALLOWED_COMMANDS: &[&str] = &["serve", "config"];
```

The `--version` and `--help` flags are handled by `clap` before the guard
runs and do not need explicit allowlist entries.

## Validation

Downstream bead br-21gj.2.4 must add a hard validation test that:

1. Attempts to run every CLI-only command via the MCP binary.
2. Asserts exit code 2 and the denial message for each.
3. Asserts exit code 0 for each allowed command.
