## Feature Flag Registry

`am flags` is the operator-facing registry for coarse feature toggles and kill switches. It is intentionally narrower than the full `Config` surface in `crates/mcp-agent-mail-core/src/config.rs`: this document covers flags that meaningfully enable, disable, or gate subsystems, not every tuning knob or numeric budget.

### Sources

- `env`: the current process environment overrides everything else
- `config`: the persisted operator config envfile, usually `~/.config/mcp-agent-mail/config.env`
- `.env`: the current working directory project envfile
- `default`: the compiled default

### Commands

```bash
am flags list
am flags list --set
am flags list --experimental
am flags list --format json
am flags status ATC_LEARNING_DISABLED
am flags explain ATC_WRITE_MODE
am flags on ATC_LEARNING_DISABLED
am flags off TUI_EFFECTS
```

`on` and `off` only work for boolean flags that are explicitly marked as dynamically writable. Static flags are still visible through `list`, `status`, and `explain`, but changing them requires editing config and restarting the affected process.

### Registered Flags

| Name | Env var | Default | Stability | Dynamic | Scope |
|------|---------|---------|-----------|---------|-------|
| `ACK_ESCALATION_ENABLED` | `ACK_ESCALATION_ENABLED` | `false` | experimental | no | Overdue-ack escalation workflows |
| `ACK_TTL_ENABLED` | `ACK_TTL_ENABLED` | `false` | stable | no | Overdue-ack scanning and warnings |
| `ATC_LEARNING_DISABLED` | `ATC_LEARNING_DISABLED` | `false` | stable | yes | ATC learning kill switch |
| `ATC_WRITE_MODE` | `AM_ATC_WRITE_MODE` | `off` | experimental | no | ATC persistence mode (`off|shadow|live`) |
| `BACKPRESSURE_SHEDDING_ENABLED` | `BACKPRESSURE_SHEDDING_ENABLED` | `false` | experimental | no | Capacity-governor shedding for low-priority reads under red health |
| `COALESCER_ADAPTIVE_FLUSH_ENABLED` | `AM_COALESCER_ADAPTIVE_FLUSH_ENABLED` | `false` | experimental | no | Adaptive archive commit-coalescer flush windows |
| `HTTP_ALLOW_LOCALHOST_UNAUTHENTICATED` | `HTTP_ALLOW_LOCALHOST_UNAUTHENTICATED` | `false` | experimental | no | Local development auth bypass |
| `LLM_ENABLED` | `LLM_ENABLED` | `false` | experimental | no | LLM-backed features |
| `NOTIFICATIONS_ENABLED` | `NOTIFICATIONS_ENABLED` | `false` | stable | no | Filesystem notification signals |
| `QUOTA_ENABLED` | `QUOTA_ENABLED` | `false` | experimental | no | Attachment and inbox quota enforcement |
| `RETENTION_REPORT_ENABLED` | `RETENTION_REPORT_ENABLED` | `false` | stable | no | Periodic retention reports |
| `RETENTION_REPORT_INTERVAL_SECONDS` | `RETENTION_REPORT_INTERVAL_SECONDS` | `3600` | stable | no | Retention/quota worker scan interval, floored at 60 seconds |
| `RETENTION_MAX_AGE_DAYS` | `RETENTION_MAX_AGE_DAYS` | `180` | stable | no | Age threshold for read-only retention reports |
| `RETENTION_IGNORE_PROJECT_PATTERNS` | `RETENTION_IGNORE_PROJECT_PATTERNS` | `demo,test*,testproj*,testproject,backendproj*,frontendproj*` | stable | no | Comma-separated project slug patterns skipped by retention reports |
| `TOOLS_FILTER_ENABLED` | `TOOLS_FILTER_ENABLED` | `false` | experimental | no | Tool-surface reduction profiles |
| `TUI_EFFECTS` | `AM_TUI_EFFECTS` | `true` | stable | yes | Ambient TUI effects |
| `TUI_ENABLED` | `TUI_ENABLED` | `true` | stable | no | Start the interactive TUI |
| `WORKTREES_ENABLED` | `WORKTREES_ENABLED` | `false` | stable | no | Product Bus and build slots |

### Notes

- `ATC_LEARNING_DISABLED` takes precedence over `ATC_WRITE_MODE`.
- `BACKPRESSURE_SHEDDING_ENABLED=false` keeps the capacity governor in shadow mode:
  robot health still reports `defer`/`downgrade` recommendations for shedable
  reads, but dispatch only rejects them when the flag is explicitly enabled.
- `COALESCER_ADAPTIVE_FLUSH_ENABLED=false` keeps the archive coalescer
  controller in shadow mode. Workers still record the recommended target window,
  the effective window, batching ratio, and max archive lag in per-repo stats;
  setting the flag to `true` makes workers use the recommended window.
- `WORKTREES_ENABLED` may also be implied by `GIT_IDENTITY_ENABLED`; the registry reports the effective state.
- `TUI_EFFECTS` and `ATC_LEARNING_DISABLED` are the first dynamically writable flags because they already use the persisted operator envfile path. The rest of the registry is intentionally read-first until more hot-reload plumbing exists.
