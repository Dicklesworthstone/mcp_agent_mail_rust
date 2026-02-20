# AgentMailTUI Operator Runbook

Practical guidance for starting, operating, troubleshooting, and recovering
the AgentMailTUI interactive operations console.

---

## 1. Quick Start

```bash
# Default: MCP transport, localhost:8765, TUI enabled
am serve-http

# API transport mode
am serve-http --path api

# Custom host/port
am serve-http --host 0.0.0.0 --port 9000

# Headless (server only, no TUI)
mcp-agent-mail serve --no-tui

# Skip authentication
am serve-http --no-auth
```

The `am serve-http` command sets `LOG_RICH_ENABLED=true` and auto-discovers
`HTTP_BEARER_TOKEN` from `~/.mcp_agent_mail/.env` (fallback:
`~/mcp_agent_mail/.env`).

### CLI vs Server Binaries (Dual-Mode)

This repo intentionally keeps **MCP server** and **CLI** command surfaces separate:

- MCP server: `mcp-agent-mail` (default: MCP stdio; `serve` for HTTP; `config` for debugging)
- CLI: `am` (built from the `mcp-agent-mail-cli` crate)

The most common mistake is trying to run CLI-only commands through the MCP server binary.
In that case, `mcp-agent-mail` should deny on `stderr` and exit with code `2` (usage error).

Examples:

```bash
# Wrong binary (denied, exit 2)
cargo run -p mcp-agent-mail -- share deploy verify-live https://example.github.io/agent-mail

# Correct binary (CLI)
cargo run -p mcp-agent-mail-cli -- --help   # runs `am`
```

Note: `am serve-http` is the native CLI command for starting the HTTP server with TUI. It is distinct from the `am` CLI subcommands used for operator tasks.

## 2. Pre-Flight Checklist

Before starting, verify:

| Check           | How to verify                                              |
|-----------------|------------------------------------------------------------|
| Port ownership  | `ss -tlnp \| grep 8765` (reuse existing Agent Mail if live) |
| Storage dir     | `ls -la ~/.mcp_agent_mail/` (writable)                     |
| Database URL    | `echo $DATABASE_URL` (defaults to in-memory)               |
| Auth token      | `cat ~/.mcp_agent_mail/.env` (has token)                   |
| Disk space      | `df -h .` (>100 MB free)                                   |

If port `8765` is already used by Agent Mail, reuse it instead of force-killing.
Use a different port only when intentionally running an isolated second server.

The server runs startup probes automatically. If any fail, it prints
remediation hints and exits. Probes check:

- **http-path**: Must start and end with `/` (e.g., `/mcp/`)
- **port**: Must be bindable (not in use, not privileged without root)
- **storage**: Directory must exist and be writable
- **database**: URL must be valid and database reachable
- **sqlite-integrity**: `PRAGMA quick_check` must pass

## 3. Keyboard Controls

### Global (always active)

| Key | Action | Notes |
|-----|--------|-------|
| `1`-`9` | Jump to screens 1-9 | Suppressed during text input |
| `0` | Jump to screen 10 | Projects screen |
| `! @ # $ %` | Jump to screens 11-15 | Contacts, Explorer, Analytics, Attachments, Archive Browser |
| `Tab` / `Shift+Tab` | Next/previous screen | Cycles through all 15 screens |
| `Ctrl+P` | Command palette | Always available outside text-entry conflicts |
| `:` | Command palette | Suppressed during text input |
| `/` | Global search deep link | Opens Search with query focus |
| `.` | Contextual action menu | Uses focused row/entity actions |
| `Ctrl+N` | Open compose overlay | Global compose panel |
| `Ctrl+Y` | Toggle toast focus mode | Enter toast navigation/dismiss mode |
| `Ctrl+T` / `Shift+T` | Cycle theme | Shows toast confirmation with theme name |
| `Ctrl+E` | Export menu | Snapshot/export overlay |
| `m` | Toggle MCP/API base path | Restarts listener on `/mcp/` or `/api/` |
| `?` | Toggle help overlay | Includes screen and global keymaps |
| `q` | Quit | Immediate when not blocked by overlays |
| `Esc` | Dismiss overlay / close toast focus | Also used for quit-confirm flow |

### Screen-Specific

Each screen has its own keybindings shown in the bottom hint bar and
accessible via `?`. Common patterns:

- `j`/`k` or `Up`/`Down` — Navigate rows
- `Space`/`v`/`A`/`C` — Toggle/extend/select-all/clear multi-select (batch-capable screens)
- `Ctrl+S`/`Ctrl+L` — Save/load filter presets (Messages, Reservations, Timeline)
- `Enter` — Inspect/open selected item (screen-dependent deep links)
- `.` — Open contextual action menu on focused row

### 3.1 Command Palette Usage

Use the palette (`Ctrl+P` or `:`) for fast, low-friction control:

1. Open palette and type fuzzy text (screen/tool/agent/project/thread/reservation).
2. Use arrow keys + `Enter` to execute actions.
3. Prefer palette when direct jump keys are ambiguous (especially screens 11-15).
4. Use palette for transport/layout/macro actions to keep workflows deterministic.

### 3.2 Toast Focus Mode

Enter toast focus mode with `Ctrl+Y` when multiple notifications are active.

| Key | Action |
|-----|--------|
| `j`/`Down` | Next toast |
| `k`/`Up` | Previous toast |
| `Enter` | Dismiss focused toast |
| `m` | Mute/unmute toasts (runtime) |
| `Esc` | Exit toast focus mode |

### 3.3 High-Signal Screen Keys

- Messages: `g` toggles Local/Global inbox mode; `c` opens compose; `Ctrl+M` marks read; `Ctrl+V` drops to thread.
- Threads: `e/c` expands/collapses all message cards; `Left/Right` collapses/expands selected branch.
- Timeline: `V` cycles Events/Commits/Combined; lowercase `v` toggles visual selection mode.
- Search: `f` focuses facet rail; use `j/k` + `Enter` to cycle scope/sort/field facets.
- Contacts: `n` toggles Table/Graph; `g` toggles Mermaid panel.
- Reservations: `n` opens create-reservation form.

## 4. Screens Reference

| # | Screen       | Purpose                                           |
|---|--------------|---------------------------------------------------|
| 1 | Dashboard    | Event stream, anomaly rail, throughput and triage summary |
| 2 | Messages     | Inbox/outbox triage, presets, compose/reply, detail pane |
| 3 | Threads      | Thread correlation and conversation drill-down |
| 4 | Agents       | Agent roster, activity, unread state, deep links |
| 5 | Search       | Query/facets/results/preview explorer |
| 6 | Reservations | Active/released file reservations, create/release workflows |
| 7 | Tool Metrics | Per-tool latency/error/call-count observability |
| 8 | SystemHealth | Probes, disk/memory, and circuit-breaker state |
| 9 | Timeline     | Events/Commits/Combined chronology + inspector |
| 10 | Projects    | Project inventory, stats, and routing helpers |
| 11 | Contacts    | Contact graph and policy management |
| 12 | Explorer    | Unified mailbox explorer with direction and ack filters |
| 13 | Analytics   | Anomaly feed with confidence and remediation links |
| 14 | Attachments | Attachment inventory, preview, and provenance |
| 15 | Archive Browser | Two-pane Git archive tree + file content preview |

### 4.1 Representative Operator Workflows

1. Incident triage from Dashboard:
   `1` → inspect anomaly rail and event log → `Enter` on high-signal event to deep-link Timeline → `9` verify sequence and timestamps.
2. Ack backlog chase:
   `2` Messages with filter/sort + presets → locate `ack_required` high/urgent traffic → pivot to `3` Threads for context → acknowledge via MCP/CLI.
3. Reservation contention diagnosis:
   `6` Reservations to identify conflicting globs/holders → `4` Agents for ownership/activity recency → `11` Contacts if policy/linking blocks direct coordination.
4. Tool latency regression check:
   `7` Tool Metrics to identify slow/failing tools → `8` SystemHealth to check DB/circuit pressure → `13` Analytics for anomaly context → capture diagnostics bundle.
5. Attachment and archive forensics:
   `14` Attachments to locate suspect payloads/messages → `15` Archive Browser for canonical file history → export evidence.

### 4.2 Cross-Project Inbox Usage (Messages)

1. Jump to Messages (`2`) and press `g` to toggle Local vs Global inbox mode.
2. In Global mode, triage by urgency/ack requirements first; then pivot to Threads (`3`) for context.
3. Save high-value filter setups with `Ctrl+S`; restore with `Ctrl+L`.
4. Use `p/P` query presets for rapid rotation between All/Urgent/Ack-focused views.

### 4.3 Advanced Search With Scope Filters

1. Open Search (`5`) and press `/` to focus the query bar.
2. Press `f` to focus the facet rail.
3. Use `j/k` to move between facets and `Enter` to cycle values.
4. Critical facets: Scope (Global/Project/Product), Sort Order (Newest/Oldest/Relevance), Field Scope (Subject/Body/Both), Importance/Ack.
5. Press `r` to reset all facets to defaults when search gets over-constrained.

### 4.4 Archive Browsing in TUI

1. Jump to Archive Browser (`%` / screen 15).
2. Navigate tree with `j/k`; use `Enter` to expand directories or preview files.
3. Use `Tab` to switch between tree and preview panes.
4. Use `Ctrl+D/U` for preview paging and `/` for filename filtering.
5. Use this view for canonical Git-backed evidence when diagnosing message/archive drift.

### 4.5 Human Overseer Message Composition

Use the web overseer form when humans need to redirect active agents quickly:

1. Open `/mail/{project}/overseer/compose`.
2. Select recipients and write subject/body (optionally set `thread_id`).
3. Submit to send a high-importance message as `HumanOverseer`.
4. Agents receive it through normal inbox resources/tools and can reply in-thread.

### 4.6 Agent Network Graph Interpretation (Contacts)

1. Open Contacts (`!` / screen 11) and press `n` for Graph mode.
2. In Graph mode, inspect node centrality (high message volume) and dense edge clusters (coordination hotspots).
3. Press `g` to toggle Mermaid panel for textual graph export/inspection.
4. Use this before large multi-agent swarms to identify overloaded communication paths.

## 5. Transport Modes

The server exposes identical tools and resources under two base paths:

| Mode | Base path | Use case                        |
|------|-----------|----------------------------------|
| MCP  | `/mcp/`   | Standard MCP protocol (default)  |
| API  | `/api/`   | Alternative REST-style routing   |

**Switch at runtime:** Press `m` or use the command palette action
"Toggle MCP/API mode". The server restarts its HTTP listener with the
new base path. Active connections are dropped and reconnect
automatically.

**Switch at startup:** `am serve-http --path api` or `HTTP_PATH=/api/`.

## 6. Configuration Reference

All configuration is via environment variables. The `Config::from_env()`
function reads them at startup. A cached singleton (`global_config()`)
is used in hot paths.

### Core

| Variable                | Default          | Description                              |
|-------------------------|------------------|------------------------------------------|
| `APP_ENVIRONMENT`       | `development`    | `development` or `production`            |
| `WORKTREES_ENABLED`     | `false`          | Enable git worktree build slot support   |
| `PROJECT_IDENTITY_MODE` | `dir`            | `dir`, `git_remote`, `git_common_dir`, `git_toplevel` |

### Database

| Variable                       | Default               | Description                      |
|--------------------------------|-----------------------|----------------------------------|
| `DATABASE_URL`                 | `sqlite:///:memory:`  | SQLite connection URL            |
| `DATABASE_POOL_SIZE`           | auto (25)             | Connection pool size             |
| `DATABASE_MAX_OVERFLOW`        | auto (75)             | Additional overflow connections   |
| `DATABASE_POOL_TIMEOUT`        | `15` (seconds)        | Pool acquisition timeout         |
| `INTEGRITY_CHECK_ON_STARTUP`   | `true`                | Run `PRAGMA quick_check` at boot |
| `INTEGRITY_CHECK_INTERVAL_HOURS` | `24`               | Periodic full integrity check    |

### HTTP Server

| Variable                              | Default      | Description                    |
|---------------------------------------|--------------|--------------------------------|
| `HTTP_HOST`                           | `127.0.0.1`  | Bind address                   |
| `HTTP_PORT`                           | `8765`        | Bind port                      |
| `HTTP_PATH`                           | `/mcp/`       | Base path                      |
| `HTTP_BEARER_TOKEN`                   | (none)        | Bearer auth token              |
| `HTTP_ALLOW_LOCALHOST_UNAUTHENTICATED`| `false`       | Skip auth for 127.0.0.1       |

### Storage

| Variable             | Default                  | Description                   |
|----------------------|--------------------------|-------------------------------|
| `STORAGE_ROOT`       | `~/.mcp_agent_mail`      | Archive root directory        |
| `GIT_AUTHOR_NAME`    | `mcp-agent-mail`         | Git commit author name        |
| `GIT_AUTHOR_EMAIL`   | `mail@agent.local`       | Git commit author email       |

### Monitoring

| Variable                        | Default | Description                        |
|---------------------------------|---------|------------------------------------|
| `DISK_SPACE_MONITOR_ENABLED`    | `true`  | Enable disk space monitoring       |
| `DISK_SPACE_WARNING_MB`         | `500`   | Warning threshold (MB)             |
| `DISK_SPACE_CRITICAL_MB`        | `100`   | Critical threshold (MB)            |
| `DISK_SPACE_FATAL_MB`           | `10`    | Fatal threshold (MB)               |
| `MEMORY_WARNING_MB`             | `2048`  | RSS warning threshold (MB)         |
| `MEMORY_CRITICAL_MB`            | `4096`  | RSS critical threshold (MB)        |
| `MEMORY_FATAL_MB`               | `8192`  | RSS fatal threshold (MB)           |

### TUI

| Variable               | Default   | Description                          |
|------------------------|-----------|--------------------------------------|
| `TUI_ENABLED`          | `true`    | Enable interactive TUI               |
| `TUI_DOCK_POSITION`    | `right`   | Dock position (`top`, `bottom`, `left`, `right`) |
| `TUI_DOCK_RATIO_PERCENT` | `40`   | Dock size as % of terminal           |
| `TUI_DOCK_VISIBLE`     | `true`    | Show dock on startup                 |
| `TUI_HIGH_CONTRAST`    | `false`   | High-contrast accessibility mode     |
| `TUI_KEY_HINTS`        | `true`    | Show key hints in status bar         |
| `TUI_REDUCED_MOTION`   | `false`   | Accessibility reduced-motion mode    |
| `TUI_SCREEN_READER`    | `false`   | Screen-reader-friendly rendering hints |
| `TUI_KEYMAP_PROFILE`   | `default` | Keymap profile (`default`/`vim`/`emacs`/`minimal`/`custom`) |
| `TUI_ACTIVE_PRESET`    | `default` | Active keymap preset name            |
| `AM_TUI_THEME`         | `default` | Theme override (`default`, `solarized`, `dracula`, `nord`, `gruvbox`, `frankenstein`) |
| `AM_TUI_TREE_STYLE`    | `rounded` | Tree style (`rounded`, `plain`, `bold`, `double`, `ascii`) |
| `AM_TUI_DEBUG`         | `false`   | Enable TUI debug behaviors           |
| `AM_TUI_EFFECTS`       | `true`    | Enable TUI visual effects            |
| `AM_TUI_AMBIENT`       | `subtle`  | Ambient rendering mode (`off`/`subtle`/`full`) |
| `AM_TUI_COACH_HINTS_ENABLED` | `true` | Enable contextual coach-hint toasts |
| `AM_TUI_TOAST_ENABLED` | `true`    | Enable toast notifications           |
| `AM_TUI_TOAST_SEVERITY` | `info`   | Toast severity floor (`info`/`warning`/`error`/`off`) |
| `AM_TUI_TOAST_POSITION` | `top-right` | Toast stack position              |
| `AM_TUI_TOAST_MAX_VISIBLE` | `3`   | Max visible toasts (1-10 clamp)     |
| `AM_TUI_TOAST_INFO_DISMISS_SECS` | `5` | Info toast timeout (seconds)   |
| `AM_TUI_TOAST_WARN_DISMISS_SECS` | `8` | Warning toast timeout (seconds) |
| `AM_TUI_TOAST_ERROR_DISMISS_SECS` | `15` | Error toast timeout (seconds) |
| `AM_TUI_THREAD_PAGE_SIZE` | `20`   | Threads screen conversation page size |
| `AM_TUI_THREAD_GUIDES` | `rounded` (theme default) | Thread tree guide style (`ascii`/`unicode`/`bold`/`double`/`rounded`) |

### Logging

| Variable                       | Default | Description                        |
|--------------------------------|---------|------------------------------------|
| `LOG_LEVEL`                    | `info`  | Minimum log level                  |
| `LOG_RICH_ENABLED`             | `false` | Colored structured output          |
| `LOG_TOOL_CALLS_ENABLED`       | `false` | Log every tool call                |
| `LOG_TOOL_CALLS_RESULT_MAX_CHARS` | `500` | Truncate tool results in logs     |
| `LOG_JSON_ENABLED`             | `false` | JSON-formatted logs                |

## 7. Troubleshooting

### Port already in use

**Symptom:** Startup probe fails with "Port 8765 is already in use"

**Fix:**
```bash
# Find the process using the port
ss -tlnp | grep 8765
# or
lsof -i :8765

# If it is already Agent Mail, reuse that server.
# Otherwise, start on a different port.
am serve-http --port 9000
```

### Database locked

**Symptom:** `database is locked` errors in logs or tool responses

**Causes:**
1. Another `mcp-agent-mail` process has the database open
2. Pool exhaustion under high load
3. Long-running transaction blocking WAL checkpointing

**Fix:**
```bash
# Check for other processes
pgrep -f mcp-agent-mail

# Increase pool size
DATABASE_POOL_SIZE=50 DATABASE_MAX_OVERFLOW=150 am serve-http

# Check for stuck WAL
sqlite3 "$DATABASE_URL" "PRAGMA wal_checkpoint(TRUNCATE);"
```

### Authentication failures

**Symptom:** Tool calls return 401 Unauthorized

**Fix:**
1. Verify token is set: `echo $HTTP_BEARER_TOKEN`
2. Check env file: `cat ~/.mcp_agent_mail/.env`
3. For local dev, use `--no-auth` or set `HTTP_ALLOW_LOCALHOST_UNAUTHENTICATED=true`

### SQLite corruption

**Symptom:** `PRAGMA integrity_check` fails at startup

**Fix:**
```bash
# Run integrity check manually
sqlite3 /path/to/storage.sqlite3 "PRAGMA integrity_check;"

# If corrupt, rebuild from archive (the archive is the source of truth):
# 1. Back up the corrupt database
cp storage.sqlite3 storage.sqlite3.corrupt

# 2. Quarantine the broken file (non-destructive)
mv storage.sqlite3 "storage.sqlite3.corrupt.$(date +%Y%m%d_%H%M%S)"

# 3. Restart — the server will create a fresh database
am serve-http
```

See also: [RECOVERY_RUNBOOK.md](../RECOVERY_RUNBOOK.md)

### No events appearing in Dashboard

**Symptom:** Dashboard shows no events or stale data

**Causes:**
1. Server not receiving requests (check port/path)
2. Poller not running (TUI disabled or crashed)
3. Event buffer overflow under extreme load

**Fix:**
1. Verify the server is reachable: `curl -s http://127.0.0.1:8765/mcp/`
2. Switch to System Health screen (`8`) to check connection probes
3. Press `r` to force refresh

### Transport mode switch fails

**Symptom:** Pressing `m` shows toast but server doesn't respond on new path

**Fix:**
1. Check logs for bind errors
2. The old port/path is released and the new one is bound. If the new path
   is invalid, the server falls back to the previous path.
3. Restart with the desired path: `am serve-http --path api`

### High memory usage

**Symptom:** RSS exceeds warning thresholds (visible in System Health screen)

**Fix:**
```bash
# Check current RSS
grep VmRSS /proc/$(pgrep -f mcp-agent-mail)/status

# Reduce pool sizes
DATABASE_POOL_SIZE=10 DATABASE_MAX_OVERFLOW=20 am serve-http

# Reduce event buffer capacity (in-memory event ring)
# Check memory pressure on System Health screen (8)
```

### Git index.lock contention

**Symptom:** `index.lock` errors in logs during high-throughput commits

The commit coalescer retries with jittered exponential backoff (up to 7
attempts) and removes stale locks older than 60 seconds as a last resort.

**Fix:**
```bash
# Check for stale locks
find ~/.mcp_agent_mail -name "index.lock" -ls

# If the owning process is dead, quarantine the stale lock:
mv ~/.mcp_agent_mail/archive/projects/<slug>/.git/index.lock \
   ~/.mcp_agent_mail/archive/projects/<slug>/.git/index.lock.stale
```

### Disk space warnings

**Symptom:** Yellow/red disk indicators in System Health screen

**Fix:**
```bash
# Check disk usage
du -sh ~/.mcp_agent_mail/

# Clean old archives
# (retention system handles this automatically if enabled)

# Adjust thresholds
DISK_SPACE_WARNING_MB=200 DISK_SPACE_CRITICAL_MB=50 am serve-http
```

### Deployment Validation (`verify-live`)

Use native verify-live for operator checks and release gates:

```bash
# Create bundle artifact (local pre-flight input)
am share export -o /tmp/agent-mail-bundle --no-zip

# Verify deployed host against the local bundle
am share deploy verify-live https://example.github.io/agent-mail \
  --bundle /tmp/agent-mail-bundle \
  --json > /tmp/verify-live.json

# Fast triage view
jq '{verdict, summary, remote_checks: [.stages.remote.checks[] | {id, severity, passed, message}]}' /tmp/verify-live.json
```

Cloudflare Pages deployment path (native artifacts + verification):

```bash
# Generate deployment tooling files, including Cloudflare workflow + wrangler template
am share deploy tooling /tmp/agent-mail-bundle

# Expected Cloudflare artifacts
ls /tmp/agent-mail-bundle/.github/workflows/deploy-cf-pages.yml
ls /tmp/agent-mail-bundle/wrangler.toml.template

# Verify a Cloudflare Pages host against the same bundle
am share deploy verify-live https://<project>.pages.dev \
  --bundle /tmp/agent-mail-bundle \
  --json > /tmp/verify-live-cf.json
```

Interpretation:
- Exit `0`: pass, or warning-only in non-strict mode.
- Exit `1`: at least one error check failed, or warning-only with `--strict`.
- Stage-level details and per-check messages are in the JSON report.

Compatibility-only wrapper behavior:
- Generated `scripts/validate_deploy.sh` should be treated as compatibility fallback.
- Prefer native command path in docs/CI/runbooks: `am share deploy verify-live`.

### Troubleshooting Suite Map (Use Before Escalation)

Search V3-specific steady-state procedures live in:
`docs/RUNBOOK-search-v3-migration.md#steady-state-operations-post-cutover`.

| Symptom / Concern | Run This Suite | Expected Artifact Root |
|-------------------|----------------|------------------------|
| MCP/API mode drift or deny behavior mismatch | `tests/e2e/test_dual_mode.sh` | `tests/artifacts/dual_mode/<timestamp>/` |
| TUI startup/bootstrap/token-redaction regressions | `tests/e2e/test_tui_startup.sh` | `tests/artifacts/tui_startup/<timestamp>/` |
| Search V3 relevance/latency/resilience drift | `tests/e2e/test_search_v3_stdio.sh`, `tests/e2e/test_search_v3_http.sh`, `tests/e2e/test_search_v3_resilience.sh`, `tests/e2e/test_search_v3_load_concurrency.sh` | `tests/artifacts/search_v3_*/<timestamp>/` |
| Browser/WASM state-sync poll+ingress contract regressions | `tests/e2e/test_tui_wasm.sh` | `tests/artifacts/tui_wasm/<timestamp>/` |
| Accessibility keyboard/contrast/reduced-motion regressions | `tests/e2e/test_tui_a11y.sh` | `tests/artifacts/tui_a11y/<timestamp>/` |
| TUI resize/reflow/screen rendering regressions | `tests/e2e/test_tui_compat_matrix.sh` | `tests/artifacts/tui_compat_matrix/<timestamp>/` |
| Explorer/analytics/widgets interaction regressions | `tests/e2e/test_tui_interactions.sh` | `tests/artifacts/tui_interactions/<timestamp>/` |
| Web UI route/action parity issues | `tests/e2e/test_mail_ui.sh` | `tests/artifacts/mail_ui/<timestamp>/` |
| Artifact bundle schema/manifest failures | `tests/e2e/test_artifacts_schema.sh` | `tests/artifacts/artifacts_schema/<timestamp>/` |
| Static export routing/search/hash parity | `tests/e2e/test_share.sh` | `tests/artifacts/share/<timestamp>/` |
| Verify-live exit-code/severity/compatibility regressions | `tests/e2e/test_share_verify_live.sh` | `tests/artifacts/share_verify_live/<timestamp>/` |
| Search V3 stale/missing evidence detection | `scripts/search_v3_evidence_freshness_check.sh --strict --output tests/artifacts/search_v3_freshness/latest.json` | `tests/artifacts/search_v3_freshness/latest.json` |

For any failing suite, validate forensic bundle structure:

```bash
source scripts/e2e_lib.sh
e2e_validate_bundle_tree tests/artifacts
```

### Structured Diagnostics Pointers (Phase 5)

| Suite | Primary Diagnostics File |
|-------|---------------------------|
| `search_v3_stdio` | `tests/artifacts/search_v3_stdio/<timestamp>/search_v3/summaries/suite_summary.json` |
| `search_v3_http` | `tests/artifacts/search_v3_http/<timestamp>/search_v3/summaries/suite_summary.json` |
| `search_v3_resilience` | `tests/artifacts/search_v3_resilience/<timestamp>/search_v3/summaries/suite_summary.json` |
| `search_v3_load_concurrency` | `tests/artifacts/search_v3_load_concurrency/<timestamp>/search_v3/summaries/suite_summary.json` |
| `search_v3_freshness_check` | `tests/artifacts/search_v3_freshness/latest.json` |
| `tui_wasm` | `tests/artifacts/tui_wasm/<timestamp>/diagnostics/wasm_scenarios.jsonl` |
| `tui_interaction` | `tests/artifacts/tui_interaction/<timestamp>/diagnostics/pty_scenarios.jsonl` |
| `tui_interactions` | `tests/artifacts/tui_interactions/<timestamp>/diagnostics/pty_scenarios.jsonl` |
| `t16_perf_gate` | `tests/artifacts/t16_perf_gate/<timestamp>/diagnostics/perf_gate_scenarios.jsonl` |

## 8. Diagnostics Collection

When reporting issues, collect the following:

```bash
# 1. Server version and build info
cargo run -p mcp-agent-mail -- --version

# 2. Configuration dump (sanitized — no tokens)
env | grep -E '^(HTTP_|DATABASE_|STORAGE_|TUI_|LOG_|DISK_|MEMORY_)' | sed 's/TOKEN=.*/TOKEN=***/'

# 3. Database health
sqlite3 /path/to/storage.sqlite3 "PRAGMA integrity_check; PRAGMA journal_mode; PRAGMA wal_checkpoint;"

# 4. Process stats
ps aux | grep mcp-agent-mail
cat /proc/$(pgrep -f mcp-agent-mail)/status | grep -E 'VmRSS|VmSize|Threads'

# 5. Disk usage
du -sh ~/.mcp_agent_mail/
df -h ~/.mcp_agent_mail/

# 6. MCP resource snapshots (if server is running)
curl -s http://127.0.0.1:8765/mcp/ -H "Authorization: Bearer $HTTP_BEARER_TOKEN" \
  -d '{"jsonrpc":"2.0","id":1,"method":"resources/read","params":{"uri":"resource://tooling/diagnostics"}}'

# 7. Recent logs
# If LOG_JSON_ENABLED=true, logs are structured and can be filtered with jq
```

### One-Command CI Artifact Retrieval + Unpack (Failing Run)

From the repository root, this command downloads the latest failed CI run
artifacts, unpacks them, and validates all discovered `bundle.json` files:

```bash
RUN_ID="$(gh run list --workflow ci.yml --status failure --limit 1 --json databaseId -q '.[0].databaseId')" && \
OUT_DIR="/tmp/am-ci-artifacts-${RUN_ID}" && \
mkdir -p "${OUT_DIR}" && \
gh run download "${RUN_ID}" -D "${OUT_DIR}" && \
bash -lc 'source scripts/e2e_lib.sh; e2e_validate_bundle_tree "'"${OUT_DIR}"'"'
```

Manual equivalent (specific run ID):

```bash
RUN_ID=<run-id>
OUT_DIR="/tmp/am-ci-artifacts-${RUN_ID}"
gh run download "${RUN_ID}" -D "${OUT_DIR}"
source scripts/e2e_lib.sh
e2e_validate_bundle_tree "${OUT_DIR}"
```

### MCP Diagnostic Resources

These resources are available via MCP `resources/read`:

| URI                                 | Content                                    |
|-------------------------------------|--------------------------------------------|
| `resource://tooling/diagnostics`    | Full diagnostic report (health, metrics)   |
| `resource://tooling/metrics`        | Per-tool call counts and latencies         |
| `resource://tooling/locks`          | Active lock state and contention info      |
| `resource://tooling/directory`      | Available tools by cluster                 |
| `resource://projects`               | All registered projects                    |
| `resource://agents/{slug}`          | Agents for a project                       |
| `resource://file_reservations/{slug}` | Active file reservations                 |

## 9. Themes

Five built-in themes are available, cycled with `Shift+T`:

1. **Cyberpunk Aurora** — Neon accents on dark background
2. **Darcula** — IntelliJ-style dark theme
3. **Lumen Light** — Light theme for bright environments
4. **Nordic Frost** — Cool blue tones
5. **High Contrast** — Maximum readability (also via `TUI_HIGH_CONTRAST=true`)

Theme selection is not persisted across restarts. Set
`CONSOLE_THEME=<name>` for a default preference.

## 10. Launch Safety Checklist

Run this before declaring a rollout candidate:

1. Security/auth:
   confirm expected auth mode (`HTTP_BEARER_TOKEN` configured unless explicitly local-only), and verify unauthorized requests are denied when auth is on.
2. Accessibility:
   verify `TUI_HIGH_CONTRAST=true` readability and key workflows (`Dashboard`, `Messages`, `Reservations`, `SystemHealth`) with keyboard-only navigation.
3. Reliability:
   run critical suites from Section 7 and keep artifact bundles for review.
4. Parity:
   verify no regressions on active parity tracks (`mail_ui`, `dual_mode`, export/share suites).
5. Rollback readiness:
   confirm fallback launch command and prior known-good commit are recorded in incident notes before go/no-go.

## 11. Deterministic Showcase Demo

Use this when you need a reproducible handoff bundle that demonstrates startup,
search/explorer, analytics/widgets, security/redaction, macro workflows/playback,
and cross-terminal compatibility in one run.

### Run

```bash
bash scripts/e2e_tui_startup.sh --showcase
```

Optional deterministic overrides:

```bash
AM_TUI_SHOWCASE_SEED=20260211 \
AM_TUI_SHOWCASE_TIMESTAMP=20260211_120000 \
bash scripts/e2e_tui_startup.sh --showcase
```

### Stage Contract (Reset/Setup/Teardown Included)

1. Reset/setup captures deterministic env and creates `tests/artifacts/tui_showcase/<timestamp>/showcase/`.
2. Suite stages run and validate expected artifacts:
   `tui_startup`, `search_cockpit`, `tui_interactions`, `security_privacy`,
   `macros`, `tui_compat_matrix`.
3. Macro playback forensics stage runs:
   `rch exec -- cargo test -p mcp-agent-mail-server operator_macro_record_save_load_replay_forensics -- --nocapture`.
4. Teardown writes handoff metadata without deleting artifacts.

### Handoff Artifacts

| Artifact | Path |
|----------|------|
| Showcase manifest | `tests/artifacts/tui_showcase/<timestamp>/showcase/manifest.json` |
| Stage index (suite + rc + log) | `tests/artifacts/tui_showcase/<timestamp>/showcase/index.tsv` |
| Deterministic replay command | `tests/artifacts/tui_showcase/<timestamp>/showcase/repro_command.txt` |
| Explorer/analytics/widgets trace | `tests/artifacts/tui_interactions/<timestamp>/trace/analytics_widgets_timeline.tsv` |
| Security/redaction evidence | `tests/artifacts/security_privacy/<timestamp>/case_06_hostile_md.txt`, `tests/artifacts/security_privacy/<timestamp>/case_09_secret_body.txt` |
| Cross-terminal profile matrix | `tests/artifacts/tui_compat_matrix/<timestamp>/profiles/tmux_screen_resize_matrix/layout_trace.tsv` |
| Macro playback forensic report | `tests/artifacts/tui/macro_replay/*_record_save_load_replay/report.json` |

### Demo Failure Recovery Appendix

| Failure | Recovery Command |
|---------|------------------|
| Missing `pyte` for PTY render emulation | `python3 -m pip install --user pyte` |
| Missing shell tools (`expect`, `tmux`, `script`) | Install required packages, then re-run showcase command. |
| A specific suite fails and you need focused rerun | `AM_TUI_SHOWCASE_SUITES=tui_interactions bash scripts/e2e_tui_startup.sh --showcase` |
| Macro playback forensic step fails | `rch exec -- cargo test -p mcp-agent-mail-server operator_macro_record_save_load_replay_forensics -- --nocapture` |
| `tui_wasm` records `RCH_REMOTE_DEP_MISMATCH` skips | `rch workers probe --all && rch status && rch queue` then rerun `tests/e2e/test_tui_wasm.sh` |
| Artifact path mismatch (wrong timestamp) | `ls -1 tests/artifacts/tui_showcase/ | tail -n 5` then open matching `showcase/index.tsv` |

## 12. Graceful Shutdown

Press `q` to initiate shutdown. The server:

1. Stops accepting new connections
2. Flushes the commit coalescer queue (waits up to 30 seconds)
3. Releases all file reservations held by this process
4. Closes the database pool
5. Exits

For immediate termination, send `SIGTERM` or `SIGINT` (Ctrl+C).

## 13. Health Levels

The System Health screen shows overall health as Green/Yellow/Red:

| Level  | Meaning                              | Action                       |
|--------|--------------------------------------|------------------------------|
| Green  | All systems operational              | None needed                  |
| Yellow | Warning thresholds exceeded          | Monitor closely              |
| Red    | Critical condition detected          | Investigate and remediate    |

At Red level, the server may shed non-essential tools to protect core
operations. Check the System Health screen for specifics.

## 14. Common Operations

### Restart without data loss
```bash
# The commit coalescer flushes on shutdown
# Just stop and start again
q  # or Ctrl+C
am serve-http
```

### Change database location
```bash
DATABASE_URL=sqlite:///path/to/new.sqlite3 am serve-http
```

### Run in production mode
```bash
APP_ENVIRONMENT=production \
  LOG_LEVEL=warn \
  LOG_JSON_ENABLED=true \
  HTTP_HOST=0.0.0.0 \
  mcp-agent-mail serve --no-tui
```

### Enable rate limiting
```bash
HTTP_RATE_LIMIT_ENABLED=true \
  HTTP_RATE_LIMIT_PER_MINUTE=1000 \
  HTTP_RATE_LIMIT_TOOLS_PER_MINUTE=500 \
  am serve-http
```

### Enable periodic integrity checks
```bash
INTEGRITY_CHECK_ON_STARTUP=true \
  INTEGRITY_CHECK_INTERVAL_HOURS=12 \
  am serve-http
```
