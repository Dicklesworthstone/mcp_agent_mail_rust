# Release Checklist

Gating criteria for releasing the dual-mode Agent Mail (MCP server + CLI).

**Primary Beads:** br-3vwi.12.1, br-3vwi.12.2
**Track:** br-3vwi.12 (Rollout governance, release gates, feedback loop)
**Last Updated:** 2026-02-13

---

## Staged Rollout Gate Matrix

- [ ] Phase 0 packet complete: CI + local validation evidence attached
- [ ] Phase 1 packet complete: 24-48h canary metrics + incident log review attached
- [ ] Phase 2 packet complete: 25%/50%/100% ring promotions each signed separately
- [ ] Phase 3 packet complete: GA sign-off + ongoing monitoring owners recorded
- [ ] Kill-switch owner for each V2 surface is named and on-call reachable
- [ ] Rollback communication template reviewed and ready for use

### Measurable Promotion Criteria (All phases)

| Gate family | Hard threshold (promotion blocker) | Machine-check source | Artifact evidence |
|-------------|------------------------------------|----------------------|-------------------|
| Unit + integration correctness | Pass rate = `100%` (`fail=0`) | `cargo test --workspace` and CI gate entry `Unit + integration tests=status:pass` | CI logs + gate report JSON |
| Dual-mode E2E correctness | Pass rate = `100%` (`fail=0`) for `E2E dual-mode` and `E2E mode matrix` | `bash scripts/e2e_dual_mode.sh`, `bash scripts/e2e_mode_matrix.sh`, and CI gate report | `tests/artifacts/dual_mode/*/run_summary.json` |
| Security/privacy | Pass rate = `100%` (`fail=0`) for `E2E security/privacy` | `bash tests/e2e/test_security_privacy.sh` and CI gate report | `tests/artifacts/security_privacy/*/*` |
| Accessibility | Pass rate = `100%` (`fail=0`) for `E2E TUI accessibility` | `bash scripts/e2e_tui_a11y.sh` and CI gate report | `tests/artifacts/tui_a11y/*/*` |
| Performance budgets | `perf_security_regressions=status:pass` and no p95 regression > `2.0x` baseline | `cargo test -p mcp-agent-mail-cli --test perf_security_regressions -- --nocapture` and CI gate report | perf/security test logs + benchmark artifacts |
| Determinism | Golden/export checks report zero mismatches | `bash scripts/bench_golden.sh validate` and static export tests | `benches/golden/checksums.sha256`, `tests/artifacts/share/*/*` |
| Automation/governance | CI report has `decision=\"go\"`, `release_eligible=true`, and sign-off row completed | `bash scripts/ci.sh --report tests/artifacts/ci/gate_report.json` | `tests/artifacts/ci/gate_report.json`, sign-off ledger row |

### Gate-to-Bead Evidence Map

| Gate family | Required bead outputs | Evidence path |
|-------------|------------------------|---------------|
| Unit/integration + harness coverage | `br-3vwi.10` track outputs (`mode_matrix_harness`, `semantic_conformance`) | CI logs + `tests/artifacts/dual_mode/*` |
| Security/privacy | `br-3vwi.10.14` security/privacy E2E suite | `tests/artifacts/security_privacy/*` |
| Accessibility | `br-3vwi.10.13` keyboard/focus/contrast suite | `tests/artifacts/tui_a11y/*` |
| Performance | `br-3vwi.10.11` perf regression script pack | perf regression logs and trend artifacts |
| Deterministic replay/export | `br-3vwi.10.19` + `br-3vwi.10.22` | replay artifacts + share/export artifacts |
| Rollout governance + operator readiness | `br-3vwi.11.1` + `br-3vwi.12.1` + `br-3vwi.12.2` | `docs/ROLLOUT_PLAYBOOK.md`, `docs/RELEASE_CHECKLIST.md`, CI gate report |

## Functional Readiness

- [x] `am` starts server + TUI with one command
- [x] `am --no-tui` runs headless server
- [x] `am --api` / `am --mcp` switches transport modes
- [x] `am --no-auth` disables authentication for local dev
- [x] Auth token auto-discovered from `~/.mcp_agent_mail/.env`
- [x] All 34 MCP tools respond correctly
- [x] All 20+ MCP resources return correct data
- [x] Startup probes catch and report common failures (port, storage, DB)
- [x] Graceful shutdown flushes commit queue
- [x] Native deploy verification path available: `am share deploy verify-live <url> --bundle <bundle_dir>`

## Dual-Mode Interface (ADR-001)

- [x] MCP binary (`mcp-agent-mail`) denies CLI-only commands with exit 2
- [x] CLI binary (`am`) accepts all 22+ command families
- [x] Denial message includes command name, allowed commands, and remediation hint
- [x] No env variable (`INTERFACE_MODE`, etc.) can bypass the denial gate
- [x] Case variants of allowed commands are denied (e.g., `Serve`, `CONFIG`)
- [x] `mcp-agent-mail serve --help` exits 0
- [x] `mcp-agent-mail config` exits 0
- [x] All CLI parity commands implemented (messaging, contacts, reservations, agents, tooling)

## TUI Screens

- [x] Dashboard: event stream, sparkline, counters
- [x] Messages: browse, search, filter
- [x] Threads: correlation, drill-down
- [x] Agents: roster with recency indicators
- [x] Reservations: TTL countdowns, status
- [x] Tool Metrics: per-tool latency, call counts
- [x] System Health: connection probes, disk/memory
- [x] Command palette (Ctrl+P) with all actions
- [x] Help overlay (?) with screen-specific keybindings
- [x] Theme cycling (Shift+T) across 5 themes
- [x] MCP/API mode toggle (m)

## Tests

- [x] Workspace tests pass (`cargo test` — 1000+ tests)
- [x] Conformance tests pass (`cargo test -p mcp-agent-mail-conformance`)
- [x] Clippy clean: `cargo clippy --workspace -- -D warnings`
- [x] Format clean: `cargo fmt --all -- --check`
- [x] No keybinding conflicts (automated test)
- [x] E2E: `am` starts and reaches ready state
- [x] E2E: TUI interaction flows (search, timeline, palette)
- [x] E2E: MCP/API mode switching
- [x] E2E: stdio transport
- [x] E2E: CLI commands
- [x] E2E: verify-live failure matrix + compatibility-wrapper delegation checks
- [x] Stress tests pass (concurrent agents, pool exhaustion)

### Dual-Mode Test Suites

- [x] Mode matrix harness: 22 CLI-allow + 16 MCP-deny + 2 MCP-allow
  ```bash
  cargo test -p mcp-agent-mail-cli --test mode_matrix_harness
  ```
- [x] Semantic conformance: 10 SC tests (DB parity, validation, drift report)
  ```bash
  cargo test -p mcp-agent-mail-cli --test semantic_conformance
  ```
- [x] Perf/security regressions: 13 tests (latency budgets, bypass attempts)
  ```bash
  cargo test -p mcp-agent-mail-cli --test perf_security_regressions
  ```
- [x] Help snapshots match golden fixtures
  ```bash
  cargo test -p mcp-agent-mail-cli --test help_snapshots
  ```
- [x] E2E dual-mode: 84+ assertions (7 sections)
  ```bash
  bash scripts/e2e_dual_mode.sh
  ```
- [x] E2E mode matrix: 42+ assertions
  ```bash
  bash scripts/e2e_mode_matrix.sh
  ```

## Performance

- [x] Startup probes complete in <2 seconds
- [x] Event ring buffer bounded (no memory leak under load)
- [x] Commit coalescer batches effectively under load
- [x] DB pool sized appropriately (25 + 75 overflow)
- [x] No sustained lock contention at steady state

## Documentation

- [x] README includes dual-mode interface section
- [x] Operator runbook: startup, controls, troubleshooting, diagnostics
- [x] Developer guide: adding screens, actions, keybindings, tests
- [x] Recovery runbook: SQLite corruption, archive rebuild
- [x] ADR-001: dual-mode invariants documented
- [x] Migration guide: before/after command mappings
- [x] Legacy script shim deprecation/rollback policy documented (`docs/SPEC-script-migration-matrix.md`, T10.5)
- [x] Rollout playbook: phased plan + kill-switch procedure
- [x] AGENTS.md: dual-mode reminder for agents
- [x] Verify-live contract + compatibility strategy documented (`docs/SPEC-verify-live-contract.md`)

## Artifact Sanity Checks

Before release, verify test artifacts are consistent and complete:

```bash
# 1. Dual-mode E2E artifacts exist and show 0 failures
ls tests/artifacts/dual_mode/*/run_summary.json
cat tests/artifacts/dual_mode/*/run_summary.json
# e2e_fail must be 0, e2e_pass >= 84

# 2. No failure bundles generated
ls tests/artifacts/dual_mode/*/failures/
# Should be empty (no fail_*.json files)

# 3. Per-step structured logs exist
ls tests/artifacts/dual_mode/*/steps/step_*.json | wc -l
# Should be >= 42 (one per test step)

# 4. Golden snapshot checksums are current
bash scripts/bench_golden.sh validate
# All checksums must match

# 4b. Legacy shim emits deprecation and forwards to native command
PATH="/data/tmp/cargo-target/release:$PATH" legacy/hooks/check_inbox.sh --help
# Must print deprecation guidance, then native `am check-inbox` help output

# 5. Verify-live E2E artifacts exist (native path authoritative)
bash tests/e2e/test_share_verify_live.sh
ls tests/artifacts/share_verify_live/*/case_*/command.txt
ls tests/artifacts/share_verify_live/*/case_*/check_trace.jsonl
# If command unavailable in current binary, suite emits deterministic SKIP with reason

# 6. Golden denial fixtures exist
ls tests/fixtures/golden_snapshots/mcp_deny_*.txt
# At least 5 files (share, guard, doctor, archive, migrate)

# 7. Machine-readable gate report exists and is release-eligible
bash scripts/ci.sh --report tests/artifacts/ci/gate_report.json
jq '.decision, .release_eligible, .thresholds, (.gates | length)' tests/artifacts/ci/gate_report.json
# decision must be "go", release_eligible must be true, thresholds must have zero failed_gates
```

## Rollout Validation

### Before promoting

1. Run the full CI suite (includes all dual-mode gates):
   ```bash
   bash scripts/ci.sh --report tests/artifacts/ci/gate_report.json
   ```

   Confirm gate report decision:
   ```bash
   jq '.decision' tests/artifacts/ci/gate_report.json
   # "go" required for promotion
   ```

2. Or run individual gates:
   ```bash
   cargo test --workspace
   cargo test -p mcp-agent-mail-conformance
   cargo test -p mcp-agent-mail-cli --test mode_matrix_harness
   cargo test -p mcp-agent-mail-cli --test semantic_conformance
   cargo test -p mcp-agent-mail-cli --test perf_security_regressions
   cargo test -p mcp-agent-mail-cli --test help_snapshots
   bash scripts/e2e_dual_mode.sh
   bash scripts/e2e_mode_matrix.sh
   bash tests/e2e/test_security_privacy.sh
   bash scripts/e2e_tui_a11y.sh
   ```

3. Manual smoke test:
   ```bash
   # MCP denial gate works:
   mcp-agent-mail share 2>&1        # Should exit 2 with denial message

   # CLI accepts all commands:
   am share --help                  # Should exit 0
   am doctor check --json           # Should exit 0 with JSON output

   # Native deployment validation path:
   am share deploy verify-live https://example.github.io/agent-mail --bundle /tmp/agent-mail-bundle --json > /tmp/verify-live.json
   jq '.verdict, .summary' /tmp/verify-live.json

   # MCP server starts:
   mcp-agent-mail serve --help      # Should exit 0
   ```

4. Start `am` and verify TUI:
   - TUI renders correctly (all 11 screens load)
   - Keybindings respond (Tab, 1-8, ?, q)
   - Command palette opens (Ctrl+P)
   - System Health shows green status

5. Test headless mode:
   ```bash
   scripts/am --no-tui &
   curl -s http://127.0.0.1:8765/mcp/ \
     -H "Authorization: Bearer $HTTP_BEARER_TOKEN" \
     -d '{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}'
   # Should return 34 tools
   ```

---

## Post-Release Monitoring

### Signals to Watch (First 24-48 Hours)

| Signal | How to check | Expected range | Action if abnormal |
|--------|-------------|---------------|-------------------|
| MCP denial rate | `grep "not an MCP server command" <logs>` | 0 from agents | Investigate agent config |
| CLI exit codes | Operator workflow logs | Exit 0 for all commands | Check migration guide |
| DB lock contention | `resource://tooling/locks` | No increase from baseline | Check pool sizing |
| Tool latency p95 | `resource://tooling/metrics` | Within baseline SLOs | Profile hot path |
| Disk usage growth | `du -sh ~/.mcp_agent_mail/` | Stable growth rate | Check archive retention |
| Error rate | Application logs | No new error classes | Triage by error type |

### Error Classes to Monitor

| Error class | Severity | Likely cause | Runbook action |
|-------------|----------|-------------|----------------|
| Exit code 2 from agent sessions | High | Agent invoking CLI command on MCP binary | Fix agent config |
| "not an MCP server command" in MCP logs | Medium | Misrouted command | Check binary path in config |
| "database is locked" spike | Medium | Pool exhaustion under new load | Increase pool size |
| Panic/backtrace in denial stderr | Critical | Bug in denial gate | Activate kill-switch |
| CLI command succeeds on MCP binary | Critical | Denial gate bypass | Activate kill-switch immediately |

### Escalation Path

1. **Non-critical:** File a bead, fix in next release
2. **Medium:** Fix within 24 hours, deploy hotfix
3. **High/Critical:** Execute [kill-switch procedure](ROLLOUT_PLAYBOOK.md#4-kill-switch-procedure)

### Monitoring Dashboard Queries

```bash
# Count denial gate hits in last hour
grep -c "not an MCP server command" /var/log/mcp-agent-mail/*.log

# Check for any panics
grep -i "panic\|backtrace" /var/log/mcp-agent-mail/*.log

# Tool latency percentiles (via MCP resource)
curl -s http://127.0.0.1:8765/mcp/ \
  -d '{"jsonrpc":"2.0","id":1,"method":"resources/read","params":{"uri":"resource://tooling/metrics"}}' \
  | jq '.result.contents[0].text | fromjson | .tools[] | {name, call_count, p95_ms}'

# Active locks
curl -s http://127.0.0.1:8765/mcp/ \
  -d '{"jsonrpc":"2.0","id":1,"method":"resources/read","params":{"uri":"resource://tooling/locks"}}' \
  | jq '.result.contents[0].text | fromjson | .summary'
```

## V2 Surface Ownership and Kill-Switch Mapping

| Surface | Activation boundary | Kill-switch action | Primary owner | Secondary owner |
|---------|---------------------|--------------------|---------------|-----------------|
| MCP interface mode | `AM_INTERFACE_MODE` policy + binary separation | Clear CLI mode env and redeploy MCP binary | Runtime owner | Release owner |
| CLI workflows (`am`) | CLI binary rollout ring | Roll back `am` to last-known-good release | CLI owner | Runtime owner |
| TUI console | `TUI_ENABLED=true` and launch profile | Restart with `--no-tui` | TUI owner | Runtime owner |
| Static export pipeline | publish workflow gates | Disable publish jobs and hold exports | Docs/release owner | CLI owner |
| Build slots/worktrees | `WORKTREES_ENABLED=true` only after canary | Set `WORKTREES_ENABLED=false` and restart | Runtime owner | Storage owner |
| Local auth posture | bearer/JWT policy | Re-enable strict auth (`HTTP_ALLOW_LOCALHOST_UNAUTHENTICATED=0`) | Security owner | Runtime owner |

## Incident Rollback Timelines and Communication Paths

| Milestone | Target timeline | Channel | Required payload |
|-----------|-----------------|---------|------------------|
| Incident acknowledged | <= 5 minutes | on-call chat | incident ID, suspected surface, owner |
| Kill-switch decision | <= 10 minutes | incident bridge | go/no-go with rationale |
| Rollback executed | <= 15 minutes | deployment channel | command/runbook step + env scope |
| Operator notice | <= 20 minutes | operator channel + thread | user impact + workaround |
| Evidence bundle posted | <= 60 minutes | bead thread + incident doc | logs, artifacts, reproduction |

## Release Sign-Off Ledger (Required)

Fill one row per phase promotion decision.

### Sign-Off Workflow (Required For Every Promotion)

1. Run full gates and emit report: `bash scripts/ci.sh --report tests/artifacts/ci/gate_report.json`.
2. Confirm report fields: `decision == "go"` and `release_eligible == true`.
3. Attach at least one artifact link per gate family (correctness, security/privacy, accessibility, performance, determinism).
4. Record owner, UTC timestamp, and rationale in the ledger row for the phase transition.
5. If any threshold fails, mark decision `no-go`, document blocker bead IDs, and do not promote.

### Non-Quick Candidate Cadence (br-3vwi.12.3.3)

Run a **non-quick** gate report at least once every 24 hours during active rollout and within 12 hours before any promotion decision.

```bash
run_ts="$(date -u +%Y%m%d_%H%M%S)"
bash scripts/ci.sh --report "tests/artifacts/ci/${run_ts}/case_02_report.json"
jq '.decision, .release_eligible, .summary' "tests/artifacts/ci/${run_ts}/case_02_report.json"
```

Latest non-quick artifact snapshot:
- `tests/artifacts/ci/20260213_031050/case_02_report.json`
- `decision="no-go"`, `release_eligible=false`, `summary={total:13, pass:4, fail:9, skip:0}`

Owner rotation (weekly, Monday 00:00 UTC handoff):
| Primary owner | Backup owner | Responsibility |
|---------------|--------------|----------------|
| Release owner | CI maintainer | Run non-quick report, update docs with latest artifact path + verdict, record ledger evidence links |
| CI maintainer | Agent integration lead | Verify report schema/completeness and flag stale artifact age (>24h) |
| Agent integration lead | On-call operator | Confirm rollout thread + bead updates reference latest artifact before promotion |

| Phase | Decision (`go`/`no-go`) | Owner | UTC timestamp | Rationale | Evidence links |
|------|--------------------------|-------|---------------|-----------|----------------|
| Phase 0 -> Phase 1 |  |  |  |  |  |
| Phase 1 -> Phase 2 |  |  |  |  |  |
| Phase 2 (25% -> 50%) |  |  |  |  |  |
| Phase 2 (50% -> 100%) |  |  |  |  |  |
| Phase 3 (GA confirmation) |  |  |  |  |  |

## Post-Launch Telemetry Feedback Loop (br-3vwi.12.3)

- [x] Latest release-candidate gate artifact is non-quick and stored at `tests/artifacts/ci/20260213_031050/case_02_report.json`
- [x] Current reference non-quick artifact reviewed: `tests/artifacts/ci/20260213_031050/case_02_report.json` (mode=`full`, reviewed 2026-02-13T03:17Z)
- [ ] Gate decision is `go` and `release_eligible` is `true`
- [x] Projected-vs-observed summary is updated in `docs/ROLLOUT_PLAYBOOK.md` Section 9
- [ ] Follow-up bead set is reviewed and triaged:
  - `br-3vwi.12.3.1` (SearchScope compile blockers)
  - `br-3vwi.12.3.2` (clippy rate-limiter lint blockers)
  - `br-3vwi.12.3.3` (non-quick gate cadence + artifact publication)
- [ ] Owner and timestamp recorded in the sign-off ledger row for this phase

---

## Reference

- [ADR-001: Dual-Mode Invariants](ADR-001-dual-mode-invariants.md)
- [Migration Guide](MIGRATION_GUIDE.md)
- [Rollout Playbook](ROLLOUT_PLAYBOOK.md)
- [Operator Runbook](OPERATOR_RUNBOOK.md)
- [Parity Matrix](SPEC-parity-matrix.md)
