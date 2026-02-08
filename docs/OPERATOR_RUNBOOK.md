# AgentMailTUI Operator Runbook

Practical guidance for starting, operating, troubleshooting, and recovering
the AgentMailTUI interactive operations console.

---

## 1. Quick Start

```bash
# Default: MCP transport, localhost:8765, TUI enabled
scripts/am

# API transport mode
scripts/am --api

# Custom host/port
scripts/am --host 0.0.0.0 --port 9000

# Headless (server only, no TUI)
scripts/am --no-tui

# Skip authentication
scripts/am --no-auth
```

The `am` wrapper sets `LOG_RICH_ENABLED=true` and auto-discovers
`HTTP_BEARER_TOKEN` from `~/.mcp_agent_mail/.env` (fallback:
`~/mcp_agent_mail/.env`).

Without the wrapper, invoke the binary directly:

```bash
cargo run -p mcp-agent-mail -- serve \
  --host 127.0.0.1 --port 8765 --path /mcp/
```

## 2. Pre-Flight Checklist

Before starting, verify:

| Check           | How to verify                                  |
|-----------------|------------------------------------------------|
| Port available  | `ss -tlnp \| grep 8765` (should be empty)      |
| Storage dir     | `ls -la ~/.mcp_agent_mail/` (writable)         |
| Database URL    | `echo $DATABASE_URL` (defaults to in-memory)   |
| Auth token      | `cat ~/.mcp_agent_mail/.env` (has token)       |
| Disk space      | `df -h .` (>100 MB free)                       |

The server runs startup probes automatically. If any fail, it prints
remediation hints and exits. Probes check:

- **http-path**: Must start and end with `/` (e.g., `/mcp/`)
- **port**: Must be bindable (not in use, not privileged without root)
- **storage**: Directory must exist and be writable
- **database**: URL must be valid and database reachable
- **sqlite-integrity**: `PRAGMA quick_check` must pass

## 3. Keyboard Controls

### Global (always active)

| Key         | Action            | Notes                           |
|-------------|-------------------|---------------------------------|
| `1`-`8`     | Jump to screen    | Suppressed during text input    |
| `Tab`       | Next screen       |                                 |
| `Shift+Tab` | Previous screen   |                                 |
| `m`         | Toggle MCP/API    | Restarts transport              |
| `Ctrl+P`    | Command palette   |                                 |
| `:`         | Command palette   | Suppressed during text input    |
| `T`         | Cycle theme       | Shift+T; 5 themes available     |
| `?`         | Toggle help       |                                 |
| `q`         | Quit              |                                 |
| `Esc`       | Dismiss overlay   |                                 |

### Screen-Specific

Each screen has its own keybindings shown in the bottom hint bar and
accessible via `?`. Common patterns:

- `j`/`k` or `Up`/`Down` — Navigate rows
- `Enter` — Expand/select
- `r` — Refresh data
- `/` — Search/filter
- `v` — Cycle verbosity (Dashboard, Timeline)
- `t` — Cycle type filter (Dashboard, Timeline)

## 4. Screens Reference

| # | Screen       | Purpose                                           |
|---|--------------|---------------------------------------------------|
| 1 | Dashboard    | Real-time event stream, sparkline, counters        |
| 2 | Messages     | Browse messages with search, sort, filtering       |
| 3 | Threads      | Thread view with correlation and drill-down        |
| 4 | Agents       | Registered agents with recency indicators          |
| 5 | Reservations | Active file reservations with TTL countdowns       |
| 6 | ToolMetrics  | Per-tool latency histograms and call counts        |
| 7 | SystemHealth | Connection probes, disk/memory, circuit breakers   |
| 8 | Timeline     | Chronological event timeline with inspector         |

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

**Switch at startup:** `scripts/am --api` or `HTTP_PATH=/api/`.

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
| `TUI_DOCK_POSITION`    | `bottom`  | Dock position (`top`, `bottom`, etc.)|
| `TUI_DOCK_RATIO_PERCENT` | `30`   | Dock size as % of terminal           |
| `TUI_DOCK_VISIBLE`     | `false`   | Show dock on startup                 |
| `TUI_HIGH_CONTRAST`    | `false`   | High-contrast accessibility mode     |
| `TUI_KEY_HINTS`        | `true`    | Show key hints in status bar         |

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

# Kill it or use a different port
scripts/am --port 9000
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
DATABASE_POOL_SIZE=50 DATABASE_MAX_OVERFLOW=150 scripts/am

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

# 2. Delete and let the server rebuild from migrations
rm storage.sqlite3

# 3. Restart — the server will create a fresh database
scripts/am
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
2. Switch to System Health screen (`7`) to check connection probes
3. Press `r` to force refresh

### Transport mode switch fails

**Symptom:** Pressing `m` shows toast but server doesn't respond on new path

**Fix:**
1. Check logs for bind errors
2. The old port/path is released and the new one is bound. If the new path
   is invalid, the server falls back to the previous path.
3. Restart with the desired path: `scripts/am --api`

### High memory usage

**Symptom:** RSS exceeds warning thresholds (visible in System Health screen)

**Fix:**
```bash
# Check current RSS
grep VmRSS /proc/$(pgrep -f mcp-agent-mail)/status

# Reduce pool sizes
DATABASE_POOL_SIZE=10 DATABASE_MAX_OVERFLOW=20 scripts/am

# Reduce event buffer capacity (in-memory event ring)
# Check memory pressure on System Health screen (7)
```

### Git index.lock contention

**Symptom:** `index.lock` errors in logs during high-throughput commits

The commit coalescer retries with jittered exponential backoff (up to 7
attempts) and removes stale locks older than 60 seconds as a last resort.

**Fix:**
```bash
# Check for stale locks
find ~/.mcp_agent_mail -name "index.lock" -ls

# If the owning process is dead, safe to remove:
rm ~/.mcp_agent_mail/archive/projects/<slug>/.git/index.lock
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
DISK_SPACE_WARNING_MB=200 DISK_SPACE_CRITICAL_MB=50 scripts/am
```

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

## 10. Graceful Shutdown

Press `q` to initiate shutdown. The server:

1. Stops accepting new connections
2. Flushes the commit coalescer queue (waits up to 30 seconds)
3. Releases all file reservations held by this process
4. Closes the database pool
5. Exits

For immediate termination, send `SIGTERM` or `SIGINT` (Ctrl+C).

## 11. Health Levels

The System Health screen shows overall health as Green/Yellow/Red:

| Level  | Meaning                              | Action                       |
|--------|--------------------------------------|------------------------------|
| Green  | All systems operational              | None needed                  |
| Yellow | Warning thresholds exceeded          | Monitor closely              |
| Red    | Critical condition detected          | Investigate and remediate    |

At Red level, the server may shed non-essential tools to protect core
operations. Check the System Health screen for specifics.

## 12. Common Operations

### Restart without data loss
```bash
# The commit coalescer flushes on shutdown
# Just stop and start again
q  # or Ctrl+C
scripts/am
```

### Change database location
```bash
DATABASE_URL=sqlite:///path/to/new.sqlite3 scripts/am
```

### Run in production mode
```bash
APP_ENVIRONMENT=production \
  LOG_LEVEL=warn \
  LOG_JSON_ENABLED=true \
  HTTP_HOST=0.0.0.0 \
  scripts/am --no-tui
```

### Enable rate limiting
```bash
HTTP_RATE_LIMIT_ENABLED=true \
  HTTP_RATE_LIMIT_PER_MINUTE=1000 \
  HTTP_RATE_LIMIT_TOOLS_PER_MINUTE=500 \
  scripts/am
```

### Enable periodic integrity checks
```bash
INTEGRITY_CHECK_ON_STARTUP=true \
  INTEGRITY_CHECK_INTERVAL_HOURS=12 \
  scripts/am
```
