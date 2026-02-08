# Release Checklist

Gating criteria for promoting `scripts/am` as the default workflow.

---

## Functional Readiness

- [x] `am` starts server + TUI with one command
- [x] `am --no-tui` runs headless server
- [x] `am --api` / `am --mcp` switches transport modes
- [x] `am --no-auth` disables authentication for local dev
- [x] Auth token auto-discovered from `~/.mcp_agent_mail/.env`
- [x] All 23 MCP tools respond correctly
- [x] All 20+ MCP resources return correct data
- [x] Startup probes catch and report common failures (port, storage, DB)
- [x] Graceful shutdown flushes commit queue

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
- [x] Clippy clean (no new warnings in server crate)
- [x] No keybinding conflicts (automated test)
- [x] E2E: `am` starts and reaches ready state
- [x] E2E: TUI interaction flows (search, timeline, palette)
- [x] E2E: MCP/API mode switching
- [x] E2E: stdio transport
- [x] E2E: CLI commands
- [x] Stress tests pass (concurrent agents, pool exhaustion)

## Performance

- [x] Startup probes complete in <2 seconds
- [x] Event ring buffer bounded (no memory leak under load)
- [x] Commit coalescer batches effectively under load
- [x] DB pool sized appropriately (25 + 75 overflow)
- [x] No sustained lock contention at steady state

## Documentation

- [x] README simplified around `am` one-command workflow
- [x] Operator runbook: startup, controls, troubleshooting, diagnostics
- [x] Developer guide: adding screens, actions, keybindings, tests
- [x] Recovery runbook: SQLite corruption, archive rebuild

## Rollout Validation

### Before promoting

1. Run the full test suite:
   ```bash
   cargo test
   cargo test -p mcp-agent-mail-conformance
   ```

2. Run E2E tests:
   ```bash
   tests/e2e/test_tui_interaction.sh
   tests/e2e/test_stdio.sh
   scripts/e2e_cli.sh
   ```

3. Start `am` and verify:
   - TUI renders correctly (all 8 screens load)
   - Keybindings respond (Tab, 1-8, ?, q)
   - Command palette opens (Ctrl+P)
   - System Health shows green status

4. Run a multi-agent session:
   - Start server: `scripts/am`
   - From another terminal, send test messages via MCP
   - Verify messages appear in Dashboard and Messages screens
   - Verify agent appears in Agents screen

5. Test headless mode:
   ```bash
   scripts/am --no-tui &
     curl -s http://127.0.0.1:8765/mcp/ \
       -H "Authorization: Bearer $HTTP_BEARER_TOKEN" \
       -d '{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}'
   # Should return 34 tools
   ```

### After promoting

- Monitor error rates in first 24 hours
- Check disk usage growth in `~/.mcp_agent_mail/`
- Verify no regressions in agent coordination workflows
