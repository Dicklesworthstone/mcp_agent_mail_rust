## Feature Flag Registry

`am flags` is the operator-facing registry for coarse feature toggles, kill switches, and the subsystem knobs that have bitten operators in production. It is intentionally narrower than the full `Config` surface in `crates/mcp-agent-mail-core/src/config.rs`, with one deliberate exception: the **entire** Air Traffic Control (`AM_ATC_*`) surface is registered, and a unit test (`every_atc_env_var_in_source_is_registered`) fails the build if code reads an `AM_ATC_*` variable that is missing here or from this document, README, or the operator runbook.

The registry lives in `crates/mcp-agent-mail-core/src/flags.rs` (`FLAG_REGISTRY`); this table is checked against it by `every_atc_env_var_is_documented_with_its_default`.

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
am flags list --subsystem atc        # same as: am config atc
am flags list --format json
am flags status ATC_LEARNING_DISABLED
am flags explain ATC_WRITE_MODE
am flags explain AM_ATC_EXECUTOR_MODE   # env-var names work too
am flags on ATC_LEARNING_DISABLED
am flags off TUI_EFFECTS
```

`on` and `off` only work for boolean flags that are explicitly marked as dynamically writable. Static flags are still visible through `list`, `status`, and `explain`, but changing them requires editing config and restarting the affected process.

### Registered Flags

| Name | Env var | Default | Stability | Dynamic | Scope |
|------|---------|---------|-----------|---------|-------|
| `ACK_ESCALATION_ENABLED` | `ACK_ESCALATION_ENABLED` | `false` | experimental | no | Overdue-ack escalation workflows |
| `ACK_TTL_ENABLED` | `ACK_TTL_ENABLED` | `false` | stable | no | Overdue-ack scanning and warnings |
| `ATC_ADVISORY_COOLDOWN_SECS` | `AM_ATC_ADVISORY_COOLDOWN_SECS` | `300` | stable | no | Minimum seconds between advisories to one agent (floor 10) |
| `ATC_CANARY_REPORT_DIR` | `AM_ATC_CANARY_REPORT_DIR` | `(unset)` | experimental | no | Directory holding `latest_canary_report.json` for robot/TUI |
| `ATC_CANARY_REPORT_PATH` | `AM_ATC_CANARY_REPORT_PATH` | `(unset)` | experimental | no | Exact canary perf-gate report path for robot/TUI |
| `ATC_CUSUM_DELTA` | `AM_ATC_CUSUM_DELTA` | `0.1` | experimental | no | CUSUM minimum detectable shift (finite, `> 0`) |
| `ATC_CUSUM_THRESHOLD` | `AM_ATC_CUSUM_THRESHOLD` | `5` | experimental | no | CUSUM change-point threshold (finite, `> 0`) |
| `ATC_ENABLED` | `AM_ATC_ENABLED` | `true` | stable | no | ATC master switch; `false` = passive liveness only (see README) |
| `ATC_EPROCESS_THRESHOLD` | `AM_ATC_EPROCESS_THRESHOLD` | `20` | experimental | no | E-process calibration alert threshold → safe mode (finite, `> 0`) |
| `ATC_EXECUTOR_MODE` | `AM_ATC_EXECUTOR_MODE` | `shadow` | experimental | no | Effect executor (`shadow|dry_run|canary|live`) |
| `ATC_EXPERIENCE_MAX_ROWS` | `AM_ATC_EXPERIENCE_MAX_ROWS` | `50000` | stable | no | Ceiling on raw `atc_experiences` rows (`0` disables) |
| `ATC_LEARNING_DISABLED` | `ATC_LEARNING_DISABLED` | `false` | stable | yes | ATC learning kill switch |
| `ATC_LEDGER_CAPACITY` | `AM_ATC_LEDGER_CAPACITY` | `1000` | stable | no | Evidence-ledger ring buffer entries (floor 10) |
| `ATC_POLICY_BUNDLE_PATH` | `AM_ATC_POLICY_BUNDLE_PATH` | `(unset)` | experimental | no | Liveness policy bundle JSON overriding the baseline |
| `ATC_POPULATION_LIMIT` | `AM_ATC_POPULATION_LIMIT` | `4096` | stable | no | Max agents per population sync (clamped `1..=65536`) |
| `ATC_POPULATION_RECENCY_SECS` | `AM_ATC_POPULATION_RECENCY_SECS` | `604800` | stable | no | Hydrate only agents active within this window (7 days) |
| `ATC_PROBE_INTERVAL_SECS` | `AM_ATC_PROBE_INTERVAL_SECS` | `120` | stable | no | Operator tick / liveness-probe cadence in seconds (floor 5) |
| `ATC_RETENTION_SWEEP_INTERVAL_SECS` | `AM_ATC_RETENTION_SWEEP_INTERVAL_SECS` | `900` | stable | no | Cadence of the experience-ceiling sweep (`0` disables) |
| `ATC_SAFE_MODE_RECOVERY_COUNT` | `AM_ATC_SAFE_MODE_RECOVERY_COUNT` | `20` | experimental | no | Correct predictions needed to leave safe mode (floor 1) |
| `ATC_SUMMARY_INTERVAL_SECS` | `AM_ATC_SUMMARY_INTERVAL_SECS` | `300` | stable | no | Seconds between ATC summary log lines (floor 10) |
| `ATC_SUSPICION_K` | `AM_ATC_SUSPICION_K` | `3` | experimental | no | Rhythm-liveness suspicion factor in standard deviations (finite, `> 0`) |
| `ATC_WRITE_MODE` | `AM_ATC_WRITE_MODE` | `off` | experimental | no | ATC experience-ledger persistence mode (`off|shadow|live`) |
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

- `ATC_LEARNING_DISABLED` takes precedence over `ATC_WRITE_MODE` and also forces `ATC_ENABLED` off.
- `ATC_EXECUTOR_MODE` governs mail/reservation side effects; `ATC_WRITE_MODE` governs only the
  experience ledger. The default `shadow` + `off` pair observes without writing anything durable.
  Full semantics of every `AM_ATC_*` knob, including what "passive liveness only" means when
  `ATC_ENABLED=false`, are in the README's "Air Traffic Control (ATC) configuration" table.
- `ATC_CANARY_REPORT_PATH` / `ATC_CANARY_REPORT_DIR` are read on every robot/TUI render, so they
  do not require a restart; every other `AM_ATC_*` variable is read once at server startup.
- `BACKPRESSURE_SHEDDING_ENABLED=false` keeps the capacity governor in shadow mode:
  robot health still reports `defer`/`downgrade` recommendations for shedable
  reads, but dispatch only rejects them when the flag is explicitly enabled.
- `COALESCER_ADAPTIVE_FLUSH_ENABLED=false` keeps the archive coalescer
  controller in shadow mode. Workers still record the recommended target window,
  the effective window, batching ratio, and max archive lag in per-repo stats;
  setting the flag to `true` makes workers use the recommended window.
- `WORKTREES_ENABLED` may also be implied by `GIT_IDENTITY_ENABLED`; the registry reports the effective state.
- `TUI_EFFECTS` and `ATC_LEARNING_DISABLED` are the first dynamically writable flags because they already use the persisted operator envfile path. The rest of the registry is intentionally read-first until more hot-reload plumbing exists.
