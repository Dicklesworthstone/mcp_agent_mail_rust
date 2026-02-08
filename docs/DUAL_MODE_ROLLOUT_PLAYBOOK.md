# Dual-Mode Rollout + Kill-Switch Playbook

Phased rollout plan for the dual-mode interface (MCP-first default + CLI
opt-in). Includes activation criteria, kill-switch procedures, and rollback
paths.

**ADR:** [ADR-001](ADR-001-dual-mode-invariants.md)
**Bead:** br-21gj.6.3

---

## Architecture Summary

Two binaries, one shared storage layer:

| Binary | Purpose | Accepts |
|--------|---------|---------|
| `mcp-agent-mail` | MCP server (agents) | `serve`, `config`, no-arg (stdio) |
| `mcp-agent-mail-cli` (`am`) | Operator CLI (humans) | All 22+ command families |

The MCP binary denies CLI-only commands with exit code 2 and a remediation
hint. There is no runtime mode switching — mode is determined by which binary
is executed (compile-time separation per ADR-001 Invariant 3).

---

## Phase 1: Internal Validation (Pre-Rollout)

**Criteria to enter Phase 2:**

- [ ] All CI gates pass (unit, integration, conformance, perf/security, E2E)
  ```bash
  bash scripts/ci.sh    # or: gh workflow run ci.yml
  ```
- [ ] Mode matrix harness: 22 CLI-allow + 16 MCP-deny + 2 MCP-allow pass
  ```bash
  cargo test -p mcp-agent-mail-cli --test mode_matrix_harness
  ```
- [ ] Semantic conformance: 10 SC tests pass
  ```bash
  cargo test -p mcp-agent-mail-cli --test semantic_conformance
  ```
- [ ] Perf/security: 13 tests pass, p95 < budget
  ```bash
  cargo test -p mcp-agent-mail-cli --test perf_security_regressions
  ```
- [ ] E2E dual-mode: 84+ assertions pass
  ```bash
  bash scripts/e2e_dual_mode.sh
  ```
- [ ] Help snapshots match golden fixtures
  ```bash
  cargo test -p mcp-agent-mail-cli --test help_snapshots
  ```
- [ ] Clippy clean: `cargo clippy --workspace -- -D warnings`
- [ ] Manual smoke test: start both binaries, verify denial and help

**Owner:** Development team
**Blast radius:** None (internal only)

---

## Phase 2: Canary Deployment

**Duration:** 24-48 hours
**Blast radius:** Single operator environment

### Activation

1. Deploy both binaries to one operator workstation
2. Configure MCP clients to use `mcp-agent-mail` (stdio or HTTP)
3. Configure operator workflows to use `am` (CLI binary)
4. Monitor for 24 hours

### Success Criteria

- [ ] No denial-gate false positives (legitimate MCP commands work)
- [ ] No denial-gate false negatives (CLI commands on MCP binary are denied)
- [ ] Operator workflows complete without modification
- [ ] Agent sessions function normally
- [ ] No increase in error rates in logs

### Monitoring Signals

| Signal | Where to check | Expected |
|--------|---------------|----------|
| MCP denial rate | `grep "not an MCP server command" <logs>` | 0 (agents should not hit denial gate) |
| CLI exit codes | Operator workflow logs | Exit 0 for all commands |
| DB lock contention | `resource://tooling/locks` | No increase |
| Tool latency | `resource://tooling/metrics` | Within baseline SLOs |

### Rollback Trigger

If any of these occur, execute kill-switch (see below):

- MCP denial gate produces false negatives (CLI command executes in MCP binary)
- Agent sessions fail with exit code 2 when they shouldn't
- Database corruption or lock contention spike
- Operator reports workflow breakage

---

## Phase 3: Broad Rollout

**Duration:** 1 week
**Blast radius:** All operator environments

### Activation

1. Update deployment configs to use new binaries
2. Update `scripts/am` wrapper if needed
3. Announce migration timeline to operators (see migration guide)

### Success Criteria

- [ ] All monitoring signals from Phase 2 remain stable
- [ ] No user-reported confusion about which binary to use
- [ ] CI pipeline validates both binaries on every push
- [ ] Documentation is updated and accessible

---

## Phase 4: Steady State

- Remove any backward-compatibility shims
- Mark dual-mode as the permanent architecture
- Close the br-21gj epic

---

## Kill-Switch Procedure

### When to Activate

Activate the kill-switch if:

1. **Security:** MCP denial gate is bypassed (CLI commands execute through MCP binary)
2. **Availability:** Agent sessions fail due to dual-mode changes
3. **Data integrity:** Database corruption linked to dual-mode changes
4. **User impact:** Widespread operator workflow breakage

### Steps

**Role:** Any team member with deploy access
**Time to execute:** < 5 minutes

```bash
# Step 1: Verify the issue
# Check if it's a dual-mode problem or unrelated
mcp-agent-mail share 2>&1      # Should exit 2 with denial message
am share --help 2>&1           # Should exit 0 with help text

# Step 2: If MCP denial gate is broken, swap to previous binary
# Replace mcp-agent-mail with the last known-good version
cp /path/to/backup/mcp-agent-mail /path/to/deploy/mcp-agent-mail

# Step 3: Restart MCP server processes
# (varies by deployment method)
systemctl restart mcp-agent-mail   # or: docker restart <container>

# Step 4: Verify rollback
mcp-agent-mail share 2>&1
# Expected: exit 2 with denial message (or: original behavior if pre-dual-mode)

# Step 5: Notify team
# Post in coordination channel with:
#   - What happened
#   - What binary was rolled back
#   - Link to logs/artifacts
```

### Post-Rollback

1. Collect artifacts from the failed deployment:
   ```bash
   # E2E artifacts
   ls tests/artifacts/dual_mode/
   ls tests/artifacts/cli/perf_security/
   ls tests/artifacts/cli/mode_matrix/
   ls tests/artifacts/cli/semantic_conformance/
   ```

2. Run the CI suite against the broken state to capture reproduction:
   ```bash
   bash scripts/ci.sh 2>&1 | tee ci_failure_$(date +%Y%m%d).log
   ```

3. File a bead with the reproduction command and artifact paths

4. Do not re-deploy until:
   - Root cause is identified
   - Fix is committed and passes all CI gates
   - Phase 2 canary validates the fix

---

## Decision Points Reference

| Decision | Criteria | Action |
|----------|----------|--------|
| Enter Phase 2 | All Phase 1 checks pass | Deploy to canary |
| Enter Phase 3 | 24h canary with 0 issues | Deploy broadly |
| Enter Phase 4 | 1 week stable | Remove shims |
| Kill-switch | Any trigger condition | Rollback immediately |
| Re-deploy after rollback | Root cause fixed + CI green | Return to Phase 2 |

---

## Communication Protocol

| Event | Channel | Template |
|-------|---------|----------|
| Phase 2 start | Team chat | "Starting dual-mode canary on [env]. Monitor [dashboard]." |
| Phase 3 start | Team chat + email | "Dual-mode rolling out to all environments. Migration guide: [link]." |
| Kill-switch activated | Team chat (urgent) | "ROLLBACK: dual-mode rolled back on [env]. Reason: [brief]. Investigating." |
| Post-mortem | Team meeting | Standard incident review format |

---

## Evidence Traceability

Each rollout gate references specific test artifacts:

| Gate | Artifact source | Location |
|------|-----------------|----------|
| Mode matrix | `mode_matrix_harness.rs` | `tests/artifacts/cli/mode_matrix/` |
| Semantic conformance | `semantic_conformance.rs` | `tests/artifacts/cli/semantic_conformance/` |
| Perf/security | `perf_security_regressions.rs` | `tests/artifacts/cli/perf_security/` |
| E2E dual-mode | `e2e_dual_mode.sh` | `tests/artifacts/dual_mode/` |
| Help snapshots | `help_snapshots.rs` | `tests/fixtures/cli_help/` |
| CI pipeline | `.github/workflows/ci.yml` | GitHub Actions artifacts (14-day retention) |
