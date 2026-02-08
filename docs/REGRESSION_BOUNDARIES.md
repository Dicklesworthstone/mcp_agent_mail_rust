# Regression Boundaries: Legacy Console vs TUI

Non-regression checklist for the AgentMailTUI transition. Every capability
listed here was available in the legacy `console.rs` rich output and must
remain accessible through the TUI, headless logging, or both.

---

## 1. Startup Diagnostics

| Capability                              | Legacy (console.rs)  | TUI equivalent              | Headless equivalent         |
|-----------------------------------------|----------------------|-----------------------------|-----------------------------|
| Endpoint URL                            | Startup banner       | System Health screen (7)    | Startup log line            |
| Database path (redacted)                | Startup banner       | System Health screen (7)    | Startup log line            |
| Storage root path                       | Startup banner       | System Health screen (7)    | Startup log line            |
| Auth enabled/disabled                   | Startup banner       | Status line indicator       | Startup log line            |
| Active theme name                       | Startup banner       | Status line / Shift+T       | N/A (no TUI)               |
| DB stats (projects, agents, messages)   | Startup banner       | Dashboard counters          | `resource://projects`       |
| Console capabilities detection          | Startup banner       | Implicit (ftui probes)      | N/A                         |
| Startup probe results                   | Probe failures exit  | Probe failures exit         | Same behavior               |

**Regression gate:** `scripts/am` and `scripts/am --no-tui` must both report
endpoint, database, storage, and auth status within the first 2 seconds.

## 2. Tool Call Observability

| Capability                              | Legacy (console.rs)  | TUI equivalent              | Headless equivalent         |
|-----------------------------------------|----------------------|-----------------------------|-----------------------------|
| Tool name + timestamp on call start     | Start panel (double) | Dashboard event stream      | `LOG_TOOL_CALLS_ENABLED`    |
| Project + agent context                 | Start panel          | Dashboard event fields      | Structured log fields       |
| Parameter pretty-print                  | Start panel body     | Message detail drill-down   | Log JSON payload            |
| Sensitive value masking in params       | `mask_sensitive()`   | Same function, TUI render   | Same function, log output   |
| Duration with color gradient            | End panel header     | Tool Metrics screen (6)     | Structured log `duration_ms`|
| Duration icons (fast/medium/slow)       | End panel            | Tool Metrics latency color  | Log level escalation        |
| Query count per table (top 5)           | End panel stats      | Tool Metrics screen (6)     | `resource://tooling/metrics`|
| Total query time                        | End panel stats      | Tool Metrics screen (6)     | `resource://tooling/metrics`|
| Result preview (truncated)              | End panel body       | Message detail view         | `LOG_TOOL_CALLS_RESULT_MAX_CHARS` |
| Sensitive value masking in results      | `mask_sensitive()`   | Same function               | Same function               |

**Regression gate:** With `LOG_TOOL_CALLS_ENABLED=true`, every tool
invocation must produce at minimum: tool name, duration, and query count
in either TUI event stream or structured log output.

## 3. HTTP Request Visibility

| Capability                              | Legacy (console.rs)  | TUI equivalent              | Headless equivalent         |
|-----------------------------------------|----------------------|-----------------------------|-----------------------------|
| Method + path + status + duration       | Request panel        | Dashboard event stream      | Access log line             |
| Method color-coded by verb              | Panel render         | Event kind styling          | N/A (plain text)            |
| Status color-coded by range             | Panel render         | `style_for_status()`        | N/A (plain text)            |
| Client IP                               | Panel body           | Event detail field          | Log field                   |
| Path truncation for narrow terminals    | Panel render         | Automatic (ftui layout)     | N/A                         |

**Regression gate:** HTTP requests must be visible in the Dashboard event
stream when the TUI is active, and in structured log output when headless.

## 4. Real-Time Metrics

| Capability                              | Legacy (console.rs)  | TUI equivalent              | Headless equivalent         |
|-----------------------------------------|----------------------|-----------------------------|-----------------------------|
| Request rate sparkline (60 data points) | Sparkline buffer     | Dashboard sparkline widget  | `resource://tooling/metrics`|
| Per-tool call counts                    | Query stats          | Tool Metrics screen (6)     | `resource://tooling/metrics`|
| Per-tool latency                        | Duration gradient    | Tool Metrics screen (6)     | `resource://tooling/metrics`|

**Regression gate:** Sparkline must update at regular intervals while the
TUI is running. Tool metrics must be queryable via MCP resource in both
TUI and headless modes.

## 5. Theme and Styling

| Capability                              | Legacy (console.rs)  | TUI equivalent              |
|-----------------------------------------|----------------------|-----------------------------|
| 5 named themes                          | `CONSOLE_THEME` env  | `Shift+T` cycle + env       |
| Theme applies to all rendered output    | `theme.rs` tokens    | `TuiThemePalette` mapping   |
| High-contrast accessibility mode        | HighContrast theme   | `TUI_HIGH_CONTRAST=true`    |
| JSON syntax highlighting                | Colorize functions   | N/A (structured widgets)    |

**Regression gate:** All 5 themes must produce visible, non-overlapping
colors for every TUI screen. High-contrast mode must pass the
`all_themes_produce_valid_palette` test.

## 6. Security Invariants

These must hold in both TUI and headless modes:

- [ ] Keys matching `SENSITIVE_PATTERNS` are masked with `<redacted>` in
      all rendered output (panels, logs, event fields)
- [ ] URL userinfo (passwords in database URLs) is redacted:
      `postgres://user:REDACTED@host/db`
- [ ] `project_key` and `storage_root` are explicitly allowed (not masked)
- [ ] Auth tokens never appear in event stream, logs, or TUI widgets
- [ ] Result truncation (`LOG_TOOL_CALLS_RESULT_MAX_CHARS`) applies to
      both TUI event details and log output

## 7. Output Routing

| Mode                    | Output target                    | Active when                      |
|-------------------------|----------------------------------|----------------------------------|
| TUI (default)           | ftui terminal + event ring       | `scripts/am` (TUI_ENABLED=true)  |
| Headless                | Structured log (stdout/file)     | `scripts/am --no-tui`            |
| JSON logs               | JSON-formatted log lines         | `LOG_JSON_ENABLED=true`          |
| Pipe/redirect           | Plain text (no ANSI)             | stdout is not a TTY              |

**Regression gate:** Rich console ANSI output must be suppressed when stdout
is not a TTY. TUI must not corrupt piped output.

## 8. Configuration Parity

All legacy `console_*` config fields must have TUI equivalents:

| Legacy field                        | TUI equivalent                    | Status   |
|-------------------------------------|-----------------------------------|----------|
| `log_rich_enabled`                  | Implicit (TUI active = rich)      | Done     |
| `log_tool_calls_enabled`            | Dashboard event filtering         | Done     |
| `tools_log_enabled`                 | Tool Metrics screen toggle        | Done     |
| `log_tool_calls_result_max_chars`   | Event detail truncation           | Done     |
| `console_theme`                     | `TuiThemePalette` + Shift+T       | Done     |
| `console_persist_path`              | Layout persistence (ftui)         | Done     |
| `console_auto_save`                 | `console_auto_save` (unchanged)   | Done     |
| `console_interactive_enabled`       | TUI_ENABLED                       | Done     |
| `console_ui_height_percent`         | `TUI_DOCK_RATIO_PERCENT`          | Done     |
| `console_ui_anchor`                 | `TUI_DOCK_POSITION`               | Done     |

## 9. Notification and Toast System

| Capability                              | Legacy (console.rs)  | TUI equivalent              |
|-----------------------------------------|----------------------|-----------------------------|
| Success/info toast with icon            | Toast render         | ftui notifications          |
| Warning toast with icon                 | Toast render         | ftui notifications          |
| Error toast with icon                   | Toast render         | ftui notifications          |
| Auto-dismiss after timeout              | N/A (static)         | Duration-based dismiss      |

**Regression gate:** Theme cycling, transport mode toggle, and error
conditions must all produce visible toast notifications.

## 10. Verification Procedure

Run these checks before declaring TUI parity complete:

```bash
# 1. TUI mode: all screens render
scripts/am
# Press 1-7, verify each screen loads
# Press ?, verify help overlay
# Press Ctrl+P, verify command palette
# Press Shift+T, verify theme cycles
# Press q to quit

# 2. Headless mode: structured output
scripts/am --no-tui &
SERVER_PID=$!
curl -s http://127.0.0.1:8765/mcp/ \
  -H "Authorization: Bearer $HTTP_BEARER_TOKEN" \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}'
# Should return 23 tools
kill $SERVER_PID

# 3. Sensitive value masking
LOG_TOOL_CALLS_ENABLED=true scripts/am --no-tui &
# Send a message with "token" in params
# Verify logs show <redacted> for token values

# 4. Pipe mode: no ANSI
scripts/am --no-tui 2>&1 | cat > /tmp/output.txt
# Verify no ANSI escape sequences in output

# 5. Automated tests
cargo test -p mcp-agent-mail-server --lib -- theme
cargo test -p mcp-agent-mail-server --lib -- no_screen_conflicts
cargo test -p mcp-agent-mail-server --lib -- console
```
