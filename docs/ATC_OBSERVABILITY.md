# ATC Observability Contract

Canonical reference for ATC structured tracing spans, metrics, and alerting.
All definitions here match the live implementation in `crates/mcp-agent-mail-core/src/metrics.rs`
(`AtcMetrics` struct) and `crates/mcp-agent-mail-server/src/atc.rs`.

---

## Structured Tracing Spans

Every ATC boundary operation emits a span with a stable field schema.

| Span name | Fields | Level |
|---|---|---|
| `atc.insert_experience` | experience_id, event_kind, feature_vector_size, project_slug | DEBUG |
| `atc.resolve_experience` | experience_id, outcome, latency_micros, attribution_window_micros | DEBUG |
| `atc.sweep.reservation_expiry` | rows_scanned, rows_resolved, duration_micros | INFO |
| `atc.sweep.ack_overdue` | rows_scanned, rows_resolved | INFO |
| `atc.rollup_refresh` | strata_updated, duration_micros | INFO |
| `atc.retention_compact` | rows_deleted, rollups_preserved | INFO |
| `atc.kill_switch.change` | from_state, to_state, reason, path | INFO |
| `atc.shadow.would_insert` | event_kind, from, to, thread_id, timestamp_micros | TRACE |
| `atc.decision` | policy_version, decision_kind, confidence, safe_mode_state | DEBUG |
| `atc.safe_mode.enter` | reason, triggered_by | WARN |
| `atc.safe_mode.exit` | duration, remediation | INFO |

### Filtering guidance

- Shadow events are TRACE-level to avoid flooding production logs.
- All spans include `project_slug` and `agent_name` where applicable for per-project filtering.
- Error spans include a `sampled_input` field with truncated metadata (message bodies are redacted).

---

## Metrics

Exposed via the existing `global_metrics().atc` surface (`AtcMetrics` in `metrics.rs`).

| Metric | Type | Labels | Description |
|---|---|---|---|
| `atc_experiences_written_total` | counter | event_kind | Total experience rows appended to the ledger |
| `atc_experiences_resolved_total` | counter | outcome | Experience rows transitioned to a terminal state |
| `atc_experiences_open_by_stratum` | gauge | stratum | Currently unresolved experiences per stratum |
| `atc_sweep_duration_micros` | histogram | sweep_kind | Wall-clock time of resolution sweeps |
| `atc_sweep_rows_resolved_total` | counter | sweep_kind | Rows resolved per sweep invocation |
| `atc_rollup_refresh_latency_micros` | histogram | -- | Latency of rollup refresh operations |
| `atc_retention_rows_deleted_total` | counter | -- | Rows purged by retention compaction |
| `atc_kill_switch_enabled_gauge` | gauge | -- | 1 when kill switch is active, 0 otherwise |
| `atc_decision_latency_micros` | histogram | decision_kind | End-to-end decision engine latency |
| `atc_safe_mode_transitions_total` | counter | direction (enter/exit) | Safe-mode state transitions |
| `atc_shadow_events_total` | counter | event_kind | Shadow-mode would-insert events emitted |
| `atc_derivation_errors_total` | counter | error_kind | Experience construction or append failures |

### Recording helpers

The `AtcMetrics` struct provides typed recording methods:

- `record_experience_written(event_kind, stratum)` -- increments written counter + open gauge
- `record_experience_resolved(outcome, stratum)` -- increments resolved counter + decrements open gauge
- `record_sweep(sweep_kind, duration_micros, rows_resolved)`
- `record_rollup_refresh(latency_micros)`
- `record_retention_deleted(rows_deleted)`
- `set_kill_switch_enabled(bool)` -- called by `refresh_kill_switch()` every ATC tick
- `record_decision_latency(decision_kind, latency_micros)`
- `record_safe_mode_transition(entered: bool)`
- `record_shadow_event(event_kind)` -- called in all 6 `atc_note_*` functions during shadow mode
- `record_derivation_error(error_kind)`

---

## Alerting Rules

Example Prometheus-compatible alert rules are in
[`docs/ATC_ALERTS_EXAMPLE.yaml`](ATC_ALERTS_EXAMPLE.yaml). The file covers five
operator-actionable scenarios:

| Alert | Severity | Trigger |
|---|---|---|
| `AtcKillSwitchEnabled` | warning | Kill switch sentinel file present for > 1 min |
| `AtcResolutionSweepSlowP95` | warning | Resolution sweep p95 > 250ms for 10 min |
| `AtcDerivationErrorsPresent` | critical | Any derivation errors in 10 min window |
| `AtcSafeModeEnteringFrequently` | critical | > 2 safe-mode entries in 30 min |
| `AtcOpenExperienceStratumBacklog` | warning | Any stratum > 50 open experiences for 15 min |

Operators should adapt thresholds to their deployment. The file is intentionally
small and focuses on actionability over exhaustive coverage.

---

## TUI System Health Widget

The System Health screen includes an "ATC Health" compact widget
(`system_health.rs:render_atc_health_widget`) showing:

- Writes total / resolves total (lifetime counters)
- Kill switch state (on/off)
- Safe mode state (on/off)

The widget reads from `AtcOperatorSnapshot.observability` which is populated by
`global_metrics().atc.snapshot()` on every TUI poll tick.

---

## Operator Runbook Cross-References

- [Emergency: Disable ATC Learning](OPERATOR_RUNBOOK.md#emergency-disable-atc-learning) -- kill switch + env override procedures
- [ATC Rollback Runbook](RUNBOOK-atc-rollback.md) -- 5 incident scenarios + restoration procedure
- [ATC Alerts Example](ATC_ALERTS_EXAMPLE.yaml) -- Prometheus rule templates

---

## Kill Switch Wiring

The kill switch gauge is updated every ATC tick via `refresh_kill_switch()` in `atc.rs`:

1. Check if `.atc_kill_switch` sentinel file exists in storage root
2. Swap `ATC_KILL_SWITCH` AtomicBool
3. Call `global_metrics().atc.set_kill_switch_enabled(active)`
4. Log state transitions at INFO level with `atc.kill_switch.change` event

Zero-cost on the hot path: `atc_write_mode()` reads the AtomicBool with `Relaxed` ordering.
File existence is only checked in the tick loop (~5 second interval).
