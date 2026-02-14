# Dual-Mode Rollout + Kill-Switch Playbook

Phased rollout plan for the dual-mode interface (MCP-first default + CLI
opt-in). Includes activation criteria, kill-switch procedures, and rollback
paths.

**ADR:** [ADR-001](ADR-001-dual-mode-invariants.md)
**Primary Bead:** br-3vwi.12.1
**Track:** br-3vwi.12 (Rollout governance, release gates, feedback loop)
**Last Updated:** 2026-02-11

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

## V2 Surface Cohorts and Feature-Flag Boundaries

| Surface | Activation boundary | Default state | Kill switch |
|---------|---------------------|---------------|-------------|
| MCP interface mode | `AM_INTERFACE_MODE` policy + binary separation | `mcp` | clear CLI env + restart |
| CLI workflows (`am`) | CLI binary deployment ring | enabled for operators | roll back CLI binary |
| TUI console | `TUI_ENABLED=true` | enabled in ops profile | launch `--no-tui` |
| Static export | publish workflow + share validation | disabled until Phase 2 | disable publish jobs |
| Build slots/worktrees | `WORKTREES_ENABLED=true` | disabled | set `WORKTREES_ENABLED=false` |
| Local auth posture | bearer/JWT env policy | strict auth | force strict auth flags |

## Stage Definitions (Exposure Cohorts)

| Stage | Exposure cohort | Blast radius | Minimum dwell |
|-------|-----------------|--------------|---------------|
| Phase 0 | CI + dev workstations | internal only | until all gates pass |
| Phase 1 | Canary (1 project, 1-3 agents) | single project | 24-48h |
| Phase 2 | Rings: 25% -> 50% -> 100% | proportional by ring | 72h at 25%, 48h at 50% |
| Phase 3 | General availability | full | ongoing |

## Governance Artifacts (Auditable Evidence)

Each promotion decision must include:

- gate owner
- UTC timestamp
- decision (`go`/`no-go`)
- linked evidence bundle paths

Use `docs/RELEASE_CHECKLIST.md` as the canonical sign-off ledger.

---

## Phase 0: Internal Validation (Pre-Rollout)

**Criteria to enter Phase 1:**

- [ ] All CI gates pass (unit, integration, conformance, perf/security, E2E)
  ```bash
  am ci    # or: gh workflow run ci.yml
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

**Owner:** Development + Release owners
**Blast radius:** None (internal only)

---

## Phase 1: Canary Deployment

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

## Phase 2: Broad Rollout Rings

**Duration:** 1 week
**Blast radius:** All operator environments

### Activation

1. Roll to 25% target ring and dwell 72h
2. Roll to 50% ring only after sign-off ledger entry
3. Roll to 100% ring after second sign-off entry
4. Announce migration timeline to operators (see migration guide)

### Success Criteria

- [ ] All monitoring signals from Phase 1 remain stable
- [ ] No user-reported confusion about which binary to use
- [ ] CI pipeline validates both binaries on every push
- [ ] Documentation is updated and accessible

---

## Phase 3: Steady State

- Remove any backward-compatibility shims
- Mark dual-mode as the permanent architecture
- Close the br-3vwi.12 track when all child beads are closed

---

## Kill-Switch Procedure

### When to Activate

Activate the kill-switch if:

1. **Security:** MCP denial gate is bypassed (CLI commands execute through MCP binary)
2. **Availability:** Agent sessions fail due to dual-mode changes
3. **Data integrity:** Database corruption linked to dual-mode changes
4. **User impact:** Widespread operator workflow breakage

### Steps

**Role:** Runtime owner (primary), Release owner (secondary)
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

### Kill-Switch Ownership and Response Timelines

| Step | Owner | SLA |
|------|-------|-----|
| Detection + acknowledgement | Runtime owner | <= 5 minutes |
| Decision to rollback | Runtime owner + Release owner | <= 10 minutes |
| Rollback execution | Runtime owner | <= 15 minutes |
| Operator communications | Release owner | <= 20 minutes |
| Evidence bundle + follow-up bead | Security/release delegate | <= 60 minutes |

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
   am ci 2>&1 | tee ci_failure_$(date +%Y%m%d).log
   ```

3. File a bead with the reproduction command and artifact paths

4. Do not re-deploy until:
   - Root cause is identified
   - Fix is committed and passes all CI gates
   - Phase 1 canary validates the fix

---

## Decision Points Reference

| Decision | Criteria | Action |
|----------|----------|--------|
| Enter Phase 1 | All Phase 0 checks pass | Deploy to canary |
| Enter Phase 2 | 24h canary with 0 issues | Deploy ringed rollout |
| Enter Phase 3 | 1 week stable | Confirm steady-state |
| Kill-switch | Any trigger condition | Rollback immediately |
| Re-deploy after rollback | Root cause fixed + CI green | Return to Phase 1 |

---

## Communication Protocol

| Event | Channel | Template |
|-------|---------|----------|
| Phase 1 start | Team chat | "Starting dual-mode canary on [env]. Monitor [dashboard]." |
| Phase 2 start | Team chat + email | "Dual-mode rolling out by ring (25/50/100). Migration guide: [link]." |
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
