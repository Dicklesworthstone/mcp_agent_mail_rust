# Migration Guide: Dual-Mode Interface

How to update your workflows for the dual-mode architecture (MCP server +
CLI operator binary).

**Bead:** br-21gj.6.2

---

## What Changed

Agent Mail now ships **two binaries**:

| Binary | Name | Purpose |
|--------|------|---------|
| `mcp-agent-mail` | MCP server | Agent communication (stdio/HTTP) |
| `am` (mcp-agent-mail-cli) | Operator CLI | Admin commands (share, guard, doctor, etc.) |

The MCP binary **denies** all CLI-only commands with exit code 2 and a
remediation hint. This is enforced at the binary level — there is no
environment variable or flag to override it.

---

## Before/After Command Mapping

### Starting the server

```bash
# Before (if you used a combined binary):
mcp-agent-mail serve --host 0.0.0.0 --port 8765

# After (unchanged — server commands stay in the MCP binary):
mcp-agent-mail serve --host 0.0.0.0 --port 8765

# Or use the wrapper:
scripts/am
```

### Operator commands

```bash
# Before (if you mistakenly used the server binary):
mcp-agent-mail share export --project my-proj

# After (use the CLI binary):
am share export --project my-proj

# Guard operations
am guard install my-proj
am guard check my-proj

# Doctor
am doctor check --json

# Archive
am archive create my-proj

# Schema migration
am migrate
```

### New CLI commands (added in this release)

These commands are new — they previously required MCP tool calls:

```bash
# Messaging
am mail send -p my-proj --from BlueLake --to RedFox --subject "Hello" --body "Hi"
am mail reply -p my-proj --message-id 42 --body "Got it"
am mail inbox -p my-proj -a BlueLake --json
am mail read -p my-proj --message-id 42
am mail ack -p my-proj --message-id 42 --json
am mail search -p my-proj -a BlueLake --query "keyword" --json
am mail summarize-thread -p my-proj <thread-id>

# Contacts
am contacts request -p my-proj --from BlueLake --to RedFox --reason "coordination"
am contacts respond -p my-proj -a RedFox --from BlueLake --accept
am contacts list -p my-proj -a BlueLake --json
am contacts policy -p my-proj -a BlueLake contacts_only

# File reservations
am file_reservations reserve my-proj BlueLake "src/**" --ttl 7200
am file_reservations release my-proj BlueLake --paths "src/**"
am file_reservations list my-proj --json

# Agents
am agents list --project my-proj --json
am agents register --project my-proj --name BlueLake --program claude-code --model opus-4.6

# Tooling
am tooling directory --json
am tooling schemas --json
```

---

## Automation Migration

### Shell scripts

Find and replace the binary name:

```bash
# Before:
mcp-agent-mail share export --project "$PROJ"
mcp-agent-mail guard install "$PROJ"
mcp-agent-mail doctor check --json

# After:
am share export --project "$PROJ"
am guard install "$PROJ"
am doctor check --json
```

### CI pipelines

Update your CI to build and test both binaries:

```yaml
# GitHub Actions example
- name: Build
  run: |
    cargo build -p mcp-agent-mail      # MCP server
    cargo build -p mcp-agent-mail-cli  # Operator CLI

- name: Test dual-mode
  run: bash scripts/ci.sh
```

### MCP client configuration

No changes needed. MCP clients should already point to `mcp-agent-mail`
(stdio or HTTP). This binary continues to accept `serve`, `config`, and
no-arg (stdio default).

```json
{
  "command": "mcp-agent-mail",
  "args": []
}
```

---

## Environment Variables

No new environment variables are required. The mode is determined by which
binary you execute — there is no `INTERFACE_MODE` or `MCP_MODE` variable.

All existing environment variables (`DATABASE_URL`, `STORAGE_ROOT`,
`HTTP_HOST`, `HTTP_PORT`, `HTTP_PATH`, etc.) work the same in both binaries.

---

## Common Pitfalls

### "share" is not an MCP server command

```
$ mcp-agent-mail share export
Error: "share" is not an MCP server command.

Agent Mail MCP server accepts: serve, config
For operator CLI commands, use: mcp-agent-mail-cli share
```

**Fix:** Use `am share export` instead.

### Exit code 2 in CI

If your CI script runs `mcp-agent-mail` with a CLI command and gets exit
code 2, the command was denied by the dual-mode gate.

**Fix:** Replace `mcp-agent-mail <command>` with `am <command>` in your CI
scripts.

### "command not found: am"

The `am` binary is built from the `mcp-agent-mail-cli` crate:

```bash
cargo build -p mcp-agent-mail-cli
# Binary: target/debug/am
```

Add the target directory to your PATH, or use the full path:

```bash
./target/debug/am doctor check --json
```

### Agent sessions hitting the denial gate

If an MCP agent's sessions show exit code 2 errors, the agent is mistakenly
invoking CLI commands through the MCP binary. MCP agents should only use
MCP tool calls (via JSON-RPC), never subcommands.

**Fix:** Check the agent's configuration — it should use MCP tool calls, not
shell command invocations.

---

## Troubleshooting

### Verify dual-mode is working

```bash
# MCP binary should deny CLI commands:
mcp-agent-mail share 2>&1
# Expected: stderr error + exit 2

# CLI binary should accept all commands:
am share --help
# Expected: help text + exit 0

# MCP binary should accept server commands:
mcp-agent-mail serve --help
# Expected: help text + exit 0

mcp-agent-mail config
# Expected: config dump + exit 0
```

### Run the validation suite

```bash
# Full CI suite (all gates):
bash scripts/ci.sh

# Quick (skip E2E):
bash scripts/ci.sh --quick

# Individual suites:
cargo test -p mcp-agent-mail-cli --test mode_matrix_harness
cargo test -p mcp-agent-mail-cli --test semantic_conformance
cargo test -p mcp-agent-mail-cli --test perf_security_regressions
bash scripts/e2e_dual_mode.sh
```

### Check artifacts

Test artifacts are saved under `tests/artifacts/`:

```
tests/artifacts/
├── cli/
│   ├── mode_matrix/        # Routing decision logs
│   ├── semantic_conformance/ # Parity drift reports
│   ├── perf_security/      # Latency + security reports
│   └── help/               # Snapshot diff artifacts
└── dual_mode/              # E2E step logs + failure bundles
    ├── steps/              # Per-step JSON logs
    ├── failures/           # Failure bundles with reproduction
    └── summary.json        # Aggregate summary
```

---

## FAQ

**Q: Can I use an environment variable to make the MCP binary accept CLI commands?**
A: No. The mode boundary is enforced at the binary level (compile-time
separation). This is intentional — see [ADR-001](ADR-001-dual-mode-invariants.md).

**Q: Do both binaries share the same database?**
A: Yes. Both binaries read/write the same SQLite database specified by
`DATABASE_URL`. They share the same storage layer.

**Q: What commands does the MCP binary accept?**
A: `serve` (HTTP mode), `config` (show configuration), and no subcommand
(stdio MCP mode). Everything else is denied.

**Q: Is the `scripts/am` wrapper affected?**
A: The `scripts/am` wrapper launches `mcp-agent-mail serve` (HTTP + TUI).
It is not affected by the dual-mode change. For CLI commands, use the `am`
binary directly.

---

## Reference

- [ADR-001: Dual-Mode Invariants](ADR-001-dual-mode-invariants.md)
- [Parity Matrix](SPEC-parity-matrix.md)
- [Denial UX Contract](SPEC-denial-ux-contract.md)
- [Rollout Playbook](DUAL_MODE_ROLLOUT_PLAYBOOK.md)
- [Operator Runbook](OPERATOR_RUNBOOK.md)
