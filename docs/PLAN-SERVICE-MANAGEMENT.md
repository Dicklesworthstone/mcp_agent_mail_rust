# Implementation Plan: `am service` — Cross-Platform Service Management
**Revision 4** (final — incorporates 4 rounds of fresh-eyes review + codebase analysis)

## Context

mcp-agent-mail has **no automated service registration**. The running LaunchAgent plist was created manually, with hardcoded paths to the source repo, world-readable `.env` (644), and duplicated config between env file and plist `EnvironmentVariables`. This plan produces two focused GitHub PRs:

- **PR 1**: `paths.rs` (XDG resolution) + `user_env_file_path()` probe order + chmod 600 + `--env-file` loading order fix
- **PR 2**: `am service install/uninstall/status/logs/restart` + extend `am doctor check` + `OPERATOR_RUNBOOK.md` update

---

## Contribution Strategy: Issue → PR as Illustration

**Maintainer policy** (README.md "About Contributions"):
> "I'll have Claude or Codex review submissions via `gh` and independently decide whether and how to address them."

**Real audience: maintainer's Claude/Codex** reviewing via `gh`.

### Submission Order

1. **Issue #1** (security): "env file with HTTP_BEARER_TOKEN has world-readable permissions (644)"
   - Include `ls -la ~/.mcp_agent_mail/.env` evidence
2. **PR 1** references Issue #1 — broader path/security solution (~230 lines)
3. **Issue #2** (feature): "`am service install` for automated LaunchAgent/systemd registration"
4. **PR 2** references Issue #2 — full service management (~750 lines + runbook)

### Branch Strategy

Fork `joyshmitz/mcp_agent_mail_rust`:
- `feat/standardised-paths-and-env-security` → PR 1
- `feat/service-management` → PR 2

### PR 1 Description Template

```markdown
## Security + Architecture: Standardised paths + env file security

### Problem

The env file containing the HTTP bearer token is created with world-readable permissions (644).

Evidence:
\`\`\`
$ ls -la ~/.mcp_agent_mail/.env
-rw-r--r--  1 user  staff  64 Feb 28 10:30 .env
\`\`\`

Any user on the system can read the bearer token, compromising MCP server security.
Additionally, paths are hardcoded and scattered (config in source repo, logs in /tmp), making
deployment fragile and preventing cross-platform support.

### Solution

This PR:
1. Centralizes all path resolution via new `paths.rs` module (XDG Base Directory Spec)
2. Fixes env file loading order to apply BEFORE \`Config::from_env()\`
3. Enforces chmod 600 on env file creation
4. Makes \`parse_dotenv_contents()\` public for reuse

### Breaking Changes

**None.** Backward compatible:
- Old \`~/.mcp_agent_mail/.env\` still works (probe order: XDG → old → legacy)
- Existing binaries unaffected until they run \`am service install\`

### Test Plan

\`\`\`bash
# Verify XDG paths work
cargo test -p mcp-agent-mail-core -- paths

# Verify env file permissions
echo "TEST_VAR=test" > ~/.config/mcp-agent-mail/env
ls -la ~/.config/mcp-agent-mail/env
# Expected: -rw------- (600)

# Verify all vars loaded (not just HTTP_BEARER_TOKEN)
echo "AM_HTTP_WORKER_THREADS=2" >> ~/.config/mcp-agent-mail/env
mcp-agent-mail serve --env-file ~/.config/mcp-agent-mail/env
curl http://127.0.0.1:8765/health | jq .
# Server should use 2 worker threads (verify in logs or metrics)
\`\`\`

Closes #XYZ (link to Issue #1)
```

### PR 2 Description Template

```markdown
## Feature: \`am service install\` — automated LaunchAgent/systemd registration

### Problem

Currently, deploying mcp-agent-mail requires manual plist creation on macOS or no
service management on Linux. This is error-prone, doesn't scale to Windows, and requires
users to understand platform-specific service registration.

### Solution

This PR adds \`am service install/uninstall/status/logs/restart\` commands that:
- Automatically generate and register platform-native service configs (plist/systemd/task)
- Support macOS LaunchAgent, Linux systemd --user, and Windows (NSSM or Scheduled Task)
- Manage bearer tokens, ports, and paths via env file
- Extend \`am doctor check\` with service health verification
- Migrate legacy manual plists automatically

### Breaking Changes

**None.** Existing installations continue to work. New \`am service\` commands are opt-in.

### Test Plan

\`\`\`bash
# Dry run shows generated config
am service install --dry-run | head -20

# Actual install (should be idempotent)
am service install
am service status
am service status --json

# Health check works
am service logs --lines 5

# Stop and verify no restart
launchctl bootout gui/$(id -u) com.mcp-agent-mail.server
sleep 2
am service status  # "not registered"

# Re-install and verify restart on crash
am service install
kill $(pgrep -f "am serve-http")
sleep 2
am service status  # "running" (launchd restarted it)
\`\`\`

Closes #YZA (link to Issue #2)
```

---

## Phase 1: Platform Paths + Env-File Security (→ PR 1)

### 1.1 Create `crates/mcp-agent-mail-core/src/paths.rs`

XDG Base Directory Spec. Uses `dirs::home_dir()` as base.
**Does NOT use** `dirs::config_dir()` or `dirs::data_dir()` — they return
`~/Library/Application Support/` on macOS, wrong for CLI tools.

On **Windows**: uses `dirs::config_dir()` (`%APPDATA%`) and `dirs::data_dir()` (`%LOCALAPPDATA%`).

```
~/.config/mcp-agent-mail/env           # config (env file)  ← NEW canonical
~/.local/share/mcp-agent-mail/         # data (DB, git mailbox)
~/.local/state/mcp-agent-mail/logs/    # logs
~/.local/bin/mcp-agent-mail            # binary (unchanged)
~/.local/bin/am                        # CLI binary (unchanged)
```

XDG env var override support (checked first before defaults):
- `$XDG_CONFIG_HOME/mcp-agent-mail/env`
- `$XDG_DATA_HOME/mcp-agent-mail/`
- `$XDG_STATE_HOME/mcp-agent-mail/logs/`

Key public functions:
- `pub fn config_dir() -> PathBuf`
- `pub fn data_dir() -> PathBuf`
- `pub fn state_dir() -> PathBuf`
- `pub fn bin_dir() -> PathBuf`
- `pub fn env_file_path() -> PathBuf` — `config_dir()/env`
- `pub fn database_path() -> PathBuf` — `data_dir()/storage.sqlite3`
- `pub fn log_dir() -> PathBuf` — `state_dir()/logs`

New primitive:
- `pub fn write_file_atomic(path: &Path, content: &[u8], mode: u32) -> io::Result<()>`:
  `create_dir_all(parent)` → write temp → fsync → rename → chmod.
  **Not** `write_config_atomic()` from setup.rs (JSON-specific, cannot reuse for plist XML).

**Files to modify:**
- **NEW**: `crates/mcp-agent-mail-core/src/paths.rs`
- `crates/mcp-agent-mail-core/src/lib.rs` — add `pub mod paths;`

### 1.2 Update `user_env_file_path()` in config.rs

Currently probes `~/.mcp_agent_mail/.env` and `~/mcp_agent_mail/.env`.
Add XDG path as **first candidate** (highest priority):

```rust
fn user_env_file_path() -> Option<PathBuf> {
    let home = dirs::home_dir()?;
    let candidates = [
        paths::env_file_path(),                    // NEW: ~/.config/mcp-agent-mail/env
        home.join(".mcp_agent_mail").join(".env"), // existing preferred (backward compat)
        home.join("mcp_agent_mail").join(".env"),  // existing legacy
    ];
    candidates.into_iter().find(|p| p.is_file())
}
```

Backward compat: existing users keep working at `~/.mcp_agent_mail/.env` until
`am service install` migrates to XDG location.

**Files to modify:**
- `crates/mcp-agent-mail-core/src/config.rs:1846-1853` — update probe list

### 1.3 Fix `--env-file` loading order (CRITICAL)

**Current (broken)** execution order:
```
line 360: Config::from_env()            ← reads env vars
line 398: load_env_file_value()         ← TOO LATE, Config already built
```

`set_var()` at line 398 for vars other than `HTTP_BEARER_TOKEN` has no effect
because `Config::from_env()` already ran.

**Fixed** execution order:
```
line ~355: apply_env_file(cli.env_file) ← NEW: sets ALL vars before Config
line 360:  Config::from_env()           ← now sees env file vars
line 398:  (legacy token resolution stays as safety fallback)
```

New `apply_env_file(path: Option<&str>)` fn in main.rs:
- Parses all `KEY=VALUE` via `config::parse_dotenv_contents()` (made pub in Phase 1)
- Calls `std::env::set_var(k, v)` **only if NOT already in process env** (process env takes precedence)
- Called in synchronous `fn main()` before tokio runtime init → thread-safe

**Files to modify:**
- `crates/mcp-agent-mail/src/main.rs` — new `apply_env_file()`, move call before line 360
- `crates/mcp-agent-mail-core/src/config.rs` — make `parse_dotenv_contents()` pub

### 1.4 Env file permissions (chmod 600)

`update_envfile()` in `config.rs:1903` currently doesn't set permissions.

```rust
// After atomic rename:
#[cfg(unix)] {
    use std::os::unix::fs::PermissionsExt;
    let perms = std::fs::Permissions::from_mode(0o600);
    std::fs::set_permissions(path, perms)?;
}
```

**Files to modify:**
- `crates/mcp-agent-mail-core/src/config.rs:1903` — add chmod after rename

### 1.5 DATABASE_URL phased transition

**Phase A (PR 1):** Add deprecation warning in `Config::from_env()` when `DATABASE_URL` is
a relative path. Warn once at startup: `"DATABASE_URL is relative — run 'am service install' to migrate to absolute path"`.

**Phase B:** When `am service install` runs, write absolute `DATABASE_URL` to env file.

**Phase C:** Future release changes `Config::default()` DATABASE_URL to `paths::database_path()`.

**Files to modify:**
- `crates/mcp-agent-mail-core/src/config.rs` — deprecation warning in `from_env()`

---

## Phase 2: Service Management Module (→ PR 2)

### 2.1 Create `crates/mcp-agent-mail-cli/src/service.rs`

```rust
pub struct ServiceArgs {
    pub command: ServiceCommand,
}

pub enum ServiceCommand {
    Install { dry_run: bool },
    Uninstall,
    Status { json: bool },
    Logs { follow: bool, lines: u32 },
    Restart,
    // Legacy migration: built into Install
    // Service doctor: built into `am doctor check`
}
```

#### Platform backends (trait-based):

```rust
trait ServiceBackend {
    fn install(&self, config: &ServiceConfig) -> Result<()>;
    fn uninstall(&self) -> Result<()>;
    fn status(&self) -> Result<ServiceStatus>;
    fn restart(&self) -> Result<()>;
    fn active_log_paths(&self) -> Vec<PathBuf>; // reads from active service config
}
```

Implementations:
- **macOS**: `LaunchAgentBackend` — `~/Library/LaunchAgents/com.mcp-agent-mail.server.plist`
  → `launchctl bootstrap/bootout gui/$(id -u)`
- **Linux**: `SystemdUserBackend` — `~/.config/systemd/user/mcp-agent-mail.service`
  → `systemctl --user enable/start` + `loginctl enable-linger`
- **Windows**: `WindowsServiceBackend`:
  1. If `nssm` command available: `nssm install mcp-agent-mail <binary> ...` (preferred)
  2. Fallback: Use PowerShell `Register-ScheduledTask` via `Command::new("powershell")`
     (NOT hand-written XML — PowerShell handles validation, error handling, registry updates)

  Example PowerShell approach (safer than XML):
  ```powershell
  $action = New-ScheduledTaskAction -Execute "C:\Users\...\am.exe" `
    -Argument "serve-http --no-tui --env-file ..."
  $trigger = New-ScheduledTaskTrigger -AtLogon
  $principal = New-ScheduledTaskPrincipal -UserId $env:USERNAME
  Register-ScheduledTask -TaskName "mcp-agent-mail" -Action $action `
    -Trigger $trigger -Principal $principal -Force
  ```
  Full Windows Service implementation deferred — document NSSM setup in runbook

#### Service config generation:

The plist/unit MUST contain:
- **Absolute binary path**: `paths::bin_dir().join("am")` resolved at install time
  e.g. `/home/user/.local/bin/am` — **NEVER** bare `am` (avoids ACFS alias conflict)

  **Note on moves/updates**: If user moves binary after install, plist path breaks.
  Mitigation strategies (for future enhancement):
  1. Symlink approach: `~/.local/bin/am` is always a symlink to actual binary location
     (e.g., `~/.cargo/bin/am`). Plist points to symlink, works regardless of binary moves.
  2. Runtime resolution: Plist calls wrapper script that finds binary via `which am`.

  For now: Use absolute path (simple). Document that users should NOT move binary after install.

- Args: `serve-http --no-tui --no-reuse-running --env-file <paths::env_file_path()>`
- **No `--port`/`--host`/`--path` args** — read from env file via `--env-file`
  (user changes port by editing env file, no plist regeneration)
- **No** `EnvironmentVariables` section — all config via `--env-file`
- **No** `WorkingDirectory` — binary resolves paths via `paths.rs`
- Log paths: `paths::log_dir()/stdout.log` and `paths::log_dir()/stderr.log`
- Label/Name: `com.mcp-agent-mail.server`

**macOS KeepAlive policy**:
```xml
<key>KeepAlive</key>
<dict>
    <key>SuccessfulExit</key>
    <false/>
</dict>
```
Restart ONLY on crash (non-zero exit), NOT on clean shutdown.
Allows `launchctl bootout` to permanently stop without immediate restart.

**Linux Restart= policy**: `Restart=on-failure` (same semantics)

Uses `am serve-http` for self-heal via `run_setup_self_heal_for_server()` on every start.

#### `am service install` behavior:

Pre-flight checks (in order):
1. **Env file** exists at `paths::env_file_path()` or legacy location
2. **HTTP_BEARER_TOKEN** present in env. If missing:
   ```
   ⚠ No bearer token found. Generate one now? [y/N]
   ```
   On yes:
   - Call existing `setup::generate_token()` (uses `getrandom` crate, 256-bit entropy)
   - Call existing `setup::save_token_to_env_file()` (handles atomic write + mkdir + chmod 600)
   - This reuses proven token generation logic, not reimplemented
3. **Write defaults** to env file if not present:
   `HTTP_PORT=8765`, `HTTP_HOST=127.0.0.1`, `HTTP_PATH=/mcp/`
4. **Legacy migration**: Detect `com.mcp-agent-mail` plist (without `.server`):
   - Print: `"Found legacy service config, migrating..."`
   - Stop legacy: `launchctl bootout gui/$(id -u) com.mcp-agent-mail`
   - Backup: `paths::state_dir()/backups/com.mcp-agent-mail.plist.YYYYMMDD_HHMMSS.bak`
   - Proceed with new managed install
5. **DB migration**: Detect relative `DATABASE_URL` (old working directory convention)
   - Move `./storage.sqlite3` → `paths::database_path()` with confirmation
   - Write absolute URL to env file
6. **Distribute token to MCP clients**: Call `setup::run_setup()` with token params.
+   **Setup call signature**:
+   ```rust
+   let setup_params = SetupParams {
+       host: "127.0.0.1".to_string(),  // from HTTP_HOST env var
+       port: 8765,  // from HTTP_PORT env var (parsed)
+       token: Some(token.clone()),  // bearer token from env file
+       project_dir: std::env::current_dir()?,
+       agents: None,  // use AgentPlatform::ALL (all 9 MCP clients)
+       dry_run: false,
+   };
+   setup::run_setup(&setup_params);
+   ```
+   This updates all 9 MCP client configs (Claude, Codex, Gemini, etc.) with the server URL
+   and token, making them immediately able to connect to the service.
+
+   **Safety**: `run_setup()` is idempotent. Returns `ActionOutcome::Unchanged` if config file
+   content is already correct (lines 866-867 in setup.rs). Creates files if missing, updates
+   if changed, skips if identical.

Install steps:
1. Generate service config (plist/unit) using `paths::write_file_atomic()`
2. Register with OS service manager
3. Start service
4. **Advisory health check** (configurable timeout): probe `/health` with exponential backoff
   - Default max wait: 30s (configurable via `--health-timeout N`)
   - Backoff sequence: 100ms, 200ms, 500ms, 1s, 2s, 5s, repeat...
   - Success: `✓ Service installed and healthy`
   - Timeout: `⚠ Service installed but not yet healthy. Run 'am doctor check' to diagnose.`
   - **NEVER rollback** on health timeout — config is correct, server just needs time
5. Track installation time: `paths::state_dir()/service.json` → `{"installed_at": "2026-02-28T10:30:00Z"}`
   (Version is always `env!("CARGO_PKG_VERSION")`, no need to duplicate in state file)

**Idempotent**: If already installed → stop, update config, restart.
**`--dry-run`**: Print generated config without writing.
**Zero-sudo**: All operations in user space only.
**Permission error handling**: When operations fail due to permission denied, print actionable errors:
  ```
  ✗ Cannot create ~/Library/LaunchAgents/: permission denied
    Possible fixes:
    1. Check directory permissions: ls -ld ~/Library/LaunchAgents/
    2. Create directory manually: mkdir -p ~/Library/LaunchAgents/
    3. Check file ownership: ls -l ~/Library/LaunchAgents/com.mcp-agent-mail.*
  ```
  (Similarly for env file, state dir, etc.)

#### `am service restart` behavior

Graceful restart with proper shutdown:
1. Send SIGTERM to process (or equivalent: `launchctl bootout`, `systemctl stop`)
2. Wait up to 10s for graceful shutdown (server drains in-flight requests)
3. If still running after 10s: force kill (SIGKILL / bootout)
4. Start service again
5. Probe health endpoint

Equivalent commands per platform:
- **macOS**: `launchctl bootout gui/$(id -u) com.mcp-agent-mail.server` →
   (wait for process exit) → `launchctl bootstrap`
- **Linux**: `systemctl --user restart mcp-agent-mail.service`
- **Windows**: `Stop-ScheduledTask` → `Start-ScheduledTask`

Idempotent: if service not running, just start (no-op on stop).

#### `am service status` output:

Human-readable (default):
```
Service: mcp-agent-mail
Status:  running  (pid 17152)
Uptime:  2d 4h 37m
Config:  ~/Library/LaunchAgents/com.mcp-agent-mail.server.plist
Env:     ~/.config/mcp-agent-mail/env
Health:  ✓ http://127.0.0.1:8765/health → 200 OK (12ms)
Version: 0.9.1 (installed 2026-02-28)
Logs:    ~/.local/state/mcp-agent-mail/logs/stderr.log
```

With `--json` (follows pattern of `am setup status --json`):
```json
{"status": "running", "pid": 17152, "version": "0.9.1", "health": true}
```

#### `am service logs` (reads OS-native logs, no new deps):

```
am service logs                 # last 50 lines
am service logs --follow        # tail -f / journalctl -f
am service logs --lines 200     # last 200 lines
```

Log path resolution:
- **macOS**: Parse active plist to read `StandardErrorPath`/`StandardOutPath` values.
  Handles BOTH legacy (`/tmp/mcp-agent-mail-rust-stderr.log`) and managed (`paths::log_dir()`)
  by reading from whatever the active plist specifies.
- **Linux**: `journalctl --user -u mcp-agent-mail.service --no-pager -n N`
  (subprocess call via `Command::new("journalctl")`)
- **No `tracing-appender` dependency** — OS service managers handle log capture

### 2.2 Linux: XDG_RUNTIME_DIR pre-flight check

Before any `systemctl --user` call in `SystemdUserBackend`:
1. Check `$XDG_RUNTIME_DIR` is set; if not, set to `/run/user/$(id -u)` and warn:
   ```
   ⚠ XDG_RUNTIME_DIR not set. Using /run/user/1000. Add to your shell profile:
     export XDG_RUNTIME_DIR=/run/user/$(id -u)
   ```
2. If `loginctl enable-linger` fails (not privileged): warn but continue

### 2.3 Extend `am doctor check` with 4 service checks

Add to existing `handle_doctor_check()` at `lib.rs:~8690`, alongside existing 21 checks:

- **`service_registered`**: Detect platform service config exists (plist/unit/task)
- **`service_running`**: `launchctl list | grep com.mcp-agent-mail.server` / `systemctl --user is-active`
- **`service_health`**: Probe `/health` endpoint (reuse `startup_checks::is_agent_mail_health_check()`)
- **`env_file_permissions`**: Check `paths::env_file_path()` has mode 600, warn if 644

Also extend existing `binary_resolution` check (~line 9061) to detect ACFS alias conflict:
- If `alias am` resolves to something other than `~/.local/bin/am`, warn:
  `⚠ Shell alias 'am' shadows binary. Run: unalias am`

### 2.4 Update `docs/OPERATOR_RUNBOOK.md`

Add to PR 2:
- **"Service Management" section**: `am service install/status/logs/uninstall/restart`
- **"Configuration" section**: `~/.config/mcp-agent-mail/env`, available env vars
- **"Migration from Manual Setup" section**: for users with legacy plist
- **"ACFS Integration" section**: two-phase install, alias conflict resolution

**Files to create/modify:**
- **NEW**: `crates/mcp-agent-mail-cli/src/service.rs`
- `crates/mcp-agent-mail-cli/src/lib.rs`:
  - `pub mod service;`
  - `Service(ServiceArgs)` in `Commands` enum (~line 83)
  - Dispatch arm in `execute()` (~line 1948)
  - 4+1 new checks in `handle_doctor_check()` (~line 8690)
- `docs/OPERATOR_RUNBOOK.md` — new sections

---

## Phase 3: install.sh Integration

### 3.1 Post-install: best-effort service registration

After `am setup run --yes --no-hooks` (line 2451), add:

```bash
# Use full path to bypass potential shell aliases
if [[ -x "${INSTALL_DIR}/am" ]]; then
    "${INSTALL_DIR}/am" service install 2>/dev/null || true
fi
```

### 3.2 `--easy-mode` flag

Auto-runs `am service install` with health verification. Opt-in.

### 3.3 Uninstall: stop service before binary removal

Before binary removal (~line 1484):

```bash
if [[ -x "${INSTALL_DIR}/am" ]]; then
    "${INSTALL_DIR}/am" service uninstall 2>/dev/null || true
fi
# Verify no zombie on port 8765
```

**Files to modify:**
- `install.sh` — post-install (~line 2451), uninstall (~line 1484), `--easy-mode` flag

---

## Phase 4: ACFS Integration Contract

The `alias am='cd ~/mcp_agent_mail ...'` in `acfs.zshrc:498` is a **blocking issue** for
`am service` usage in ACFS environments. Resolution is ACFS's responsibility.

**In this repo** (`am doctor check` extends):
- Detect alias conflict and print guidance (see §2.3)

**ACFS repo changes (separate PR — not in scope of this plan):**
- Remove legacy `alias am=...` from `acfs.zshrc:498`
- Update manifest verify: use `command am service status` or `~/.local/bin/am service status`
- Two-phase install: `install.sh --yes` then `command am service install`

---

## Implementation Order

| Step | What | Deps | Lines | PR |
|------|------|------|-------|----|
| 1 | `paths.rs` + `write_file_atomic()` | — | ~180 | 1 |
| 2 | Update `user_env_file_path()` probe order | Step 1 | ~15 | 1 |
| 3 | Fix `--env-file` loading order (CRITICAL) | Step 1 | ~40 | 1 |
| 4 | chmod 600 in `update_envfile()` | — | ~10 | 1 |
| 5 | DATABASE_URL deprecation warning | — | ~15 | 1 |
| 6 | `service.rs` scaffold + trait | Step 1 | ~200 | 2 |
| 7 | macOS LaunchAgent backend | Step 6 | ~250 | 2 |
| 8 | `am service install` (full flow) | Step 7 | ~200 | 2 |
| 9 | `am service status/restart/uninstall` | Step 7 | ~100 | 2 |
| 10 | `am service logs` (OS-native) | Step 7 | ~80 | 2 |
| 11 | Extend `am doctor check` (+4 checks) | Step 7 | ~120 | 2 |
| 12 | Linux systemd backend | Step 6 | ~200 | 2 |
| 13 | install.sh integration | Step 8 | ~50 | 2 |
| 14 | `OPERATOR_RUNBOOK.md` update | Step 8 | docs | 2 |
| 15 | Windows NSSM backend | Step 6 | ~150 | future |

**PR 1 = Steps 1-5 (~260 lines)**
**PR 2 = Steps 6-14 (~1200 lines + docs, macOS + Linux)**

---

## Critical Files Reference

| File | Role | Key Lines |
|------|------|-----------|
| `crates/mcp-agent-mail-core/src/paths.rs` | **NEW** — XDG path resolution + `write_file_atomic()` | — |
| `crates/mcp-agent-mail-core/src/config.rs` | `user_env_file_path()`, `update_envfile()`, `parse_dotenv_contents()` (pub) | 1846, 1903, 1971 |
| `crates/mcp-agent-mail-core/src/setup.rs` | `run_setup()` (call in service install), `write_config_atomic()` (JSON-only, NOT reused) | 910, 842 |
| `crates/mcp-agent-mail-cli/src/lib.rs` | Commands enum, execute dispatch, `handle_doctor_check()` (+5 checks) | 83, 1948, 8690, 9061 |
| `crates/mcp-agent-mail-cli/src/service.rs` | **NEW** — Service management | — |
| `crates/mcp-agent-mail/src/main.rs` | `apply_env_file()` before Config::from_env() | 355-360 |
| `crates/mcp-agent-mail-server/src/startup_checks.rs` | `is_agent_mail_health_check()` — reuse in service install | 111-217 |
| `install.sh` | Post-install hook, uninstall service stop, `--easy-mode` | 2451, 1484 |
| `docs/OPERATOR_RUNBOOK.md` | New service management sections | — |

---

## Verification Plan

### After PR 1:
```bash
cargo test -p mcp-agent-mail-core -- paths
# Verify XDG paths
ls ~/.config/mcp-agent-mail/env  # should exist after first run
# Verify permissions
ls -la ~/.config/mcp-agent-mail/env  # -rw------- (600)
# Verify env-file loads all vars (not just token)
echo "AM_HTTP_WORKER_THREADS=2" >> ~/.config/mcp-agent-mail/env
mcp-agent-mail serve --env-file ~/.config/mcp-agent-mail/env
# Server should use 2 worker threads
```

### After PR 2:
```bash
# Dry run
am service install --dry-run
# Verify absolute path in generated plist (not bare 'am')
am service install --dry-run | grep ProgramArguments
# Expected: /Users/xxx/.local/bin/am

# Actual install (migrates legacy if present)
am service install
# ✓ Service installed and healthy

# No --port in plist (PORT from env file)
cat ~/Library/LaunchAgents/com.mcp-agent-mail.server.plist | grep "\-\-port"
# Expected: no output

# Test status
am service status
am service status --json

# Test doctor shows service checks
am doctor check
# Shows: service_registered ✓, service_running ✓, service_health ✓, env_file_permissions ✓

# Test logs (reads from plist-specified path)
am service logs --lines 10

# Test idempotent
am service install  # should update, not error

# Test clean shutdown (SuccessfulExit: false means no restart)
launchctl bootout gui/$(id -u) com.mcp-agent-mail.server
sleep 2
am service status  # should show "stopped"

# Test uninstall
am service uninstall
lsof -i :8765  # empty
```

### After install.sh integration:
```bash
./install.sh --easy-mode --yes
am service status  # running
./install.sh --uninstall --yes
lsof -i :8765  # empty, no zombie
```

---

## Out of Scope

- Changing `am` binary name (alias conflict is ACFS's responsibility)
- Removing `dirs` crate (stays, `paths.rs` builds on `dirs::home_dir()`)
- Changing default paths for existing users without migration (Phase C = future)
- Docker/container deployment
- Remote deployment / SSH
- `tracing-appender` (OS service managers handle log capture)
- Standalone `am service doctor` (extends existing `am doctor check`)
- Standalone `am service migrate` (built into `am service install`)
- Windows full Service impl (document NSSM, defer code to future PR)
