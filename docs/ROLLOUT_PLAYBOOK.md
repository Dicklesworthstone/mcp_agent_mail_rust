# Dual-Mode Rollout and Kill-Switch Playbook

**Primary Bead:** br-3vwi.12.1
**Track:** br-3vwi.12 (Rollout governance, release gates, feedback loop)
**Depends on:** security/privacy E2E, stress/soak harness, CI gate automation, dual-mode invariants
**Last Updated:** 2026-02-11

---

## 1. Overview

This playbook covers the phased rollout of the dual-mode interface
(`mcp-agent-mail` for MCP, `mcp-agent-mail-cli` for operator CLI) and the
kill-switch procedure for rolling back if incidents occur.

**Key invariant:** MCP mode is the default. The MCP binary rejects CLI-only
commands with exit code 2 and a remediation message. There is no runtime mode
switch (see ADR-001).

### 1.1 V2 Surface Cohorts and Feature-Flag Boundaries

| Surface | Cohort progression | Activation boundary | Kill switch |
|---------|--------------------|---------------------|-------------|
| MCP server interface mode | Phase 0 → 1 → 2 → 3 | Default `mcp`; CLI opt-in via `AM_INTERFACE_MODE=cli` only when explicitly needed | Remove/clear `AM_INTERFACE_MODE`, restart server |
| Operator CLI (`am`) workflows | Phase 0 → 1 → 2 → 3 | Operator binary path + command allowlist | Roll back `am` binary to last known good |
| TUI operations console | Phase 0 → 1 → 2 → 3 | `TUI_ENABLED=true` and `scripts/am` profile | Start headless with `--no-tui` |
| Web parity surfaces (`/mail/*`) | Phase 0 → 1 → 2 → 3 | Deployment cohort (host/project ring), no hidden compatibility shim | Roll back server binary and redeploy previous release |
| Static export (GH Pages / Cloudflare Pages) | Phase 0 → 1 → 2 → 3 | Publish workflow gating + `am share export` verification | Disable publish jobs and hold new exports |
| Build slots / worktree behavior | Phase 0 → 1 → 2 → 3 | `WORKTREES_ENABLED=true` only after canary evidence | Set `WORKTREES_ENABLED=false`, restart |
| Local auth strictness | Phase 0 → 1 → 2 → 3 | `HTTP_BEARER_TOKEN` + `HTTP_ALLOW_LOCALHOST_UNAUTHENTICATED` policy | Re-enable strict auth (`HTTP_ALLOW_LOCALHOST_UNAUTHENTICATED=0`) |

### 1.2 Stage Definitions (Exposure Cohorts)

| Stage | Exposure cohort | Blast radius | Minimum dwell time |
|-------|-----------------|--------------|--------------------|
| Phase 0 | CI + dev workstations | Internal only | Until all gate checks pass |
| Phase 1 | Single canary project / 1-3 active agents | One project | 24-48h |
| Phase 2 | Staged rollout rings (25% → 50% → 100%) | Proportional by ring | 72h at 25%, 48h at 50% |
| Phase 3 | General availability | Full | Ongoing |

### 1.3 Governance Artifacts (Versioned + Auditable)

Every promotion decision must reference versioned evidence in git:

- `docs/ROLLOUT_PLAYBOOK.md` (this staged policy)
- `docs/RELEASE_CHECKLIST.md` (gate state and sign-off ledger)
- `docs/DUAL_MODE_ROLLOUT_PLAYBOOK.md` (dual-mode specific operational runbook)
- `tests/artifacts/**/bundle.json` and `summary.json` outputs from required suites

Sign-off entries must include: gate owner, UTC timestamp, decision (`go`/`no-go`),
and evidence artifact paths.

---

## 2. Pre-Rollout Gate Checks

All gates must pass before advancing to any phase. Run these from the project
root.

### 2.1 Unit and Integration Tests

```bash
cargo test --workspace
# Expected: 1000+ tests, 0 failures
```

### 2.2 Clippy (Zero Warnings)

```bash
cargo clippy --workspace --all-targets
# Expected: 0 errors, 0 warnings
```

### 2.3 Conformance Tests

```bash
cargo test -p mcp-agent-mail-conformance
# Expected: all 23 tool + 20+ resource assertions pass
```

### 2.4 Dual-Mode E2E Suite

```bash
bash scripts/e2e_dual_mode.sh
# Expected: 84 assertions, 0 failures across 7 sections
# Artifacts: tests/artifacts/dual_mode/<timestamp>/
```

Verify the summary artifact:
```bash
cat tests/artifacts/dual_mode/*/run_summary.json
# e2e_fail must be 0
```

### 2.5 Mode Matrix E2E Suite

```bash
bash scripts/e2e_mode_matrix.sh
# Expected: all CLI-allow and MCP-deny assertions pass
```

### 2.6 Golden Snapshot Validation

```bash
bash scripts/bench_golden.sh validate
# Expected: all golden outputs match stored checksums
```

### 2.7 CLI Functional E2E

```bash
bash scripts/e2e_cli.sh
# Expected: 99 assertions, 0 failures
```

### 2.8 Stress Tests

```bash
cargo test -p mcp-agent-mail-db --test stress -- --nocapture
# Expected: all 9 stress scenarios pass (concurrent agents, pool exhaustion, etc.)
```

### 2.9 CI Pipeline

If `.github/workflows/ci.yml` is active, verify the latest main branch CI run
shows green across all jobs: `build`, `test`, `clippy`, `conformance`, `e2e`.

```bash
gh run list --branch main --limit 1
```

### 2.10 Security and Privacy Gates

```bash
bash tests/e2e/test_security_privacy.sh
# Expected: 0 failures
```

Required evidence:
- `tests/artifacts/security_privacy/<timestamp>/case_01_search_scope.txt`
- `tests/artifacts/security_privacy/<timestamp>/case_09_secret_body.txt`

### 2.11 Accessibility and Keyboard-Only Gates

```bash
bash tests/e2e/test_tui_a11y.sh
# Expected: 0 failures
```

### 2.12 Static Export Conformance Gates

```bash
bash tests/e2e/test_share.sh
# Expected: 0 failures with deterministic bundle manifests
```

### 2.13 Performance and Determinism Gates

```bash
bash tests/e2e/test_soak_harness.sh --quick
cargo test -p mcp-agent-mail-cli --test perf_security_regressions -- --nocapture
# Expected: no regressions, no budget failures
```

### 2.14 Measurable Gate Thresholds

Promotion from each phase requires all of:

- Test correctness: all required suites above report `fail=0`.
- Security/privacy: no leakage assertions fail in security/privacy suite artifacts.
- Accessibility: keyboard-only suite passes with no focus/contrast regressions.
- Performance: no perf/security budget test failures; no p95 regressions above 2x baseline.
- Operational readiness: release checklist sign-off ledger completed for the phase.

---

## 3. Phased Rollout Plan

### Phase 0: Internal Validation (Current)

**Scope:** Development and CI environments only.
**Blast radius:** Zero external users.
**Duration:** Until all gate checks pass.

| Criterion | Evidence |
|-----------|----------|
| All unit tests pass | `cargo test` output |
| Dual-mode E2E passes | `tests/artifacts/dual_mode/*/run_summary.json` |
| Golden snapshots stable | `bench_golden.sh validate` |
| Denial messages match contract | `tests/fixtures/golden_snapshots/mcp_deny_*.txt` |

**Exit criteria:** All 2.1-2.9 gates green. Proceed to Phase 1.

### Phase 1: Canary Deployment

**Scope:** Single project with a small agent pool (1-3 agents).
**Blast radius:** One project's messaging and reservations.
**Duration:** 24-48 hours.

**Activation steps:**
1. Deploy the new binaries to the canary host.
2. Restart the MCP server: `scripts/am`
3. Verify the server starts without probe failures.
4. Run a smoke test:
   ```bash
   # MCP binary rejects CLI commands
   mcp-agent-mail share 2>&1 | grep "is not an MCP server command"
   echo $?  # must be 2

   # CLI binary works
   am doctor check --json | jq .status  # must be "healthy"
   ```
5. Monitor for 24 hours (see Section 5).

**Rollback trigger:** Any of:
- MCP server crashes or returns non-JSON-RPC on stdout
- Denial message format deviates from golden snapshot
- Agent coordination failures (messages not delivered, reservations lost)
- Exit code other than 0 or 2 from MCP binary on known inputs

**Exit criteria:** 24 hours clean. Proceed to Phase 2.

### Phase 2: Staged Rollout

**Scope:** All projects, incremental (25% → 50% → 100%).
**Blast radius:** Proportional to rollout percentage.
**Duration:** 1 week total (3 days at 25%, 2 days at 50%, then 100%).

**Activation steps:**
1. Deploy to 25% of hosts. Monitor 72 hours.
2. If clean, deploy to 50%. Monitor 48 hours.
3. If clean, deploy to 100%.

**Monitoring at each stage:**
- Error rate in logs (grep for `exit_code=1` or panic traces)
- Agent messaging latency (tool metrics via `resource://tooling/metrics`)
- File reservation conflicts (unexpected force-releases)
- Disk usage growth rate in `~/.mcp_agent_mail/`

**Rollback trigger:** Same as Phase 1, plus:
- Error rate > 1% of tool calls
- P95 latency regression > 2x baseline
- Any agent reports inability to communicate

### Phase 3: General Availability

**Scope:** All environments, all users.
**Duration:** Ongoing.

**Post-GA actions:**
1. Remove legacy binary aliases (if any).
2. Update external documentation and integration guides.
3. Close the br-3vwi.12 rollout-governance track.

---

## 4. Kill-Switch Procedure

### 4.1 Decision Criteria

Initiate kill-switch if ANY of:

| Signal | Threshold | Detection |
|--------|-----------|-----------|
| MCP stdout corruption | Any non-JSON-RPC on stdout | Agent integration failures |
| Denial path failure | Exit code != 2 for denied command | E2E monitor or user report |
| Crash rate | > 0.1% of server starts | Process monitor |
| Message delivery failure | > 1% of sends | Tool metrics |
| Reservation integrity | Any orphaned or phantom locks | Guard check or user report |

### 4.2 Rollback Steps

**Owner:** On-call operator.
**Time target:** < 15 minutes from decision to rollback complete.

1. **Stop new deploys:**
   ```bash
   # If using deployment automation, halt the pipeline
   # If manual, skip to step 2
   ```

2. **Revert to previous binary version:**
   ```bash
   # Option A: Git revert to last known-good commit
   git log --oneline -5  # identify the pre-dual-mode commit
   git checkout <known-good-sha> -- crates/mcp-agent-mail/src/
   git checkout <known-good-sha> -- crates/mcp-agent-mail-cli/src/
   cargo build --release -p mcp-agent-mail -p mcp-agent-mail-cli

   # Option B: If pre-built binaries are archived
   cp /path/to/backup/mcp-agent-mail /usr/local/bin/
   cp /path/to/backup/am /usr/local/bin/
   ```

3. **Restart affected servers:**
   ```bash
   # Graceful restart (flushes commit queue)
   # Send SIGTERM, wait for clean exit, then restart
   pkill -TERM -f "mcp-agent-mail serve"
   sleep 5
   scripts/am
   ```

4. **Verify rollback:**
   ```bash
   # Server is responding
   curl -sf http://127.0.0.1:8765/mcp/ > /dev/null

   # Doctor passes
   am doctor check --json | jq .status
   # Expected: "healthy"
   ```

5. **Notify stakeholders:**
   - Post in the project coordination channel
   - File a bead documenting the incident and rollback reason

### 4.3 Post-Rollback Analysis

After rollback, before re-attempting rollout:

1. Reproduce the failure locally using the dual-mode E2E suite:
   ```bash
   bash scripts/e2e_dual_mode.sh
   ```

2. Check the structured step logs for the failing scenario:
   ```bash
   cat tests/artifacts/dual_mode/*/steps/step_*.json | jq 'select(.passed == false)'
   ```

3. Check failure bundles for reproduction commands:
   ```bash
   cat tests/artifacts/dual_mode/*/failures/*.json | jq .reproduction
   ```

4. Fix the root cause, add a regression test, and re-run all gate checks
   (Section 2) before re-entering the rollout phases.

---

## 5. Monitoring Checklist

Run these checks continuously during Phase 1-2. After Phase 3, incorporate
into routine operational monitoring.

### 5.1 Health Probes (Every 5 Minutes)

```bash
# Server responding
curl -sf http://127.0.0.1:8765/mcp/ -o /dev/null

# Doctor check
am doctor check --json 2>/dev/null | jq -e '.status == "healthy"'
```

### 5.2 Denial Path Integrity (Every Hour)

```bash
# Verify MCP binary still denies CLI commands correctly
for cmd in share guard doctor archive migrate; do
  exit_code=0
  mcp-agent-mail "$cmd" 2>/dev/null || exit_code=$?
  [ "$exit_code" -eq 2 ] || echo "ALERT: $cmd returned $exit_code (expected 2)"
done
```

### 5.3 Tool Metrics (Every 15 Minutes)

```bash
# Check for error rate spikes via MCP resource
curl -s http://127.0.0.1:8765/mcp/ \
  -H "Authorization: Bearer $HTTP_BEARER_TOKEN" \
  -d '{"jsonrpc":"2.0","id":1,"method":"resources/read","params":{"uri":"resource://tooling/metrics"}}' \
  | jq '.result.contents[0].text | fromjson | .tools[] | select(.error_count > 0)'
```

### 5.4 Log Scanning (Continuous)

```bash
# Watch for panics, unexpected exits, or corruption signals
tail -f /var/log/mcp-agent-mail.log | grep -iE 'panic|fatal|corrupt|SIGABRT'
```

### 5.5 Artifact Preservation

After each E2E run, archive the structured artifacts:
```bash
# Artifacts include per-step JSON logs with exit codes, stdout/stderr,
# and failure bundles with reproduction commands
ls tests/artifacts/dual_mode/*/
# steps/step_*.json  - per-assertion structured logs
# failures/fail_*.json - failure bundles (empty if all pass)
# run_summary.json - aggregate pass/fail/skip counts
```

---

## 6. Dry-Run Simulation

Before Phase 1, operators should execute a full dry-run to validate the
rollback path.

### 6.1 Simulate Deployment

```bash
# Build both binaries
cargo build -p mcp-agent-mail -p mcp-agent-mail-cli

# Start server
scripts/am &
SERVER_PID=$!
sleep 3

# Verify server is healthy
am doctor check --json | jq .status

# Run dual-mode E2E
bash scripts/e2e_dual_mode.sh
```

### 6.2 Simulate Failure and Rollback

```bash
# Kill the server (simulating crash)
kill -9 $SERVER_PID

# Verify server is down
curl -sf http://127.0.0.1:8765/mcp/ && echo "STILL UP" || echo "DOWN - OK"

# Restart (simulating rollback to same version)
scripts/am &
sleep 3

# Verify recovery
am doctor check --json | jq .status

# Verify denial path still works post-restart
mcp-agent-mail share 2>&1 | grep "is not an MCP server command"
echo "Exit code: $?"
```

### 6.3 Record Dry-Run Results

```bash
# Save dry-run evidence
mkdir -p tests/artifacts/dry_run
date -u +%Y-%m-%dT%H:%M:%SZ > tests/artifacts/dry_run/timestamp.txt
bash scripts/e2e_dual_mode.sh
cp tests/artifacts/dual_mode/*/run_summary.json tests/artifacts/dry_run/
```

---

## 7. Role Ownership

| Role | Responsibility |
|------|---------------|
| **Release owner** | Decides go/no-go at each phase gate |
| **On-call operator** | Executes kill-switch within 15 min SLA |
| **CI maintainer** | Ensures gate check automation is green |
| **Agent integration lead** | Validates agent behavior during canary |

---

## 8. Evidence Traceability

| Artifact | Source Bead | Location |
|----------|------------|----------|
| Dual-mode E2E results | br-3vwi.12.1 (and prior dual-mode work) | `tests/artifacts/dual_mode/*/` |
| CI gate logs | br-3vwi.12.1 (and prior CI gate work) | `.github/workflows/ci.yml` outputs |
| Golden snapshots | br-3vwi.12.1 (and prior snapshot work) | `tests/fixtures/golden_snapshots/` |
| Denial UX contract | br-3vwi.12.1 (rollout gate reference) | `docs/SPEC-denial-ux-contract.md` |
| Mode invariants | br-3vwi.12.1 (rollout gate reference) | `docs/ADR-001-dual-mode-invariants.md` |
| Parity matrix | br-3vwi.12.1 (rollout gate reference) | `docs/SPEC-parity-matrix.md` |
| Golden benchmark checksums | br-3vwi.12.1 (rollout gate reference) | `benches/golden/checksums.sha256` |
