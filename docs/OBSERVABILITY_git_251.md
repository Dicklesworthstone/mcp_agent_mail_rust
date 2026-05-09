# Git 2.51 Observability Contract

spec_version: 1.0.0

This document is the single source of truth for event names, baseline fields,
metric labels, and schema locations for the git 2.51.0 mitigation work. The
contract is intentionally narrow: it covers git shell-out locking, cooperative
flock behavior, segfault retry signals, reservation activity probes,
pathspec walks, orphan-ref recovery, and the health/boot checks that consume
those signals.

## 1. Naming Conventions

- Tracing target prefix: `mcp_agent_mail::<component>::<subcomponent>`.
- Event names are `snake_case` and describe a verb or state, for example
  `flock_acquired`, `git_segfault_retry`, and
  `reservation_activity_result`.
- Field names are `snake_case`.
- Units are encoded in field names: `duration_ms`, `wait_seconds`,
  `size_bytes`. Do not use a bare field such as `duration`.
- Raw git arguments are never logged by default. Use `args_hash`; raw args are
  only allowed when `AM_LOG_GIT_ARGS=1`.
- Use stable, tokenized identifiers such as `repo_slug` in operator-facing
  events instead of high-cardinality absolute paths when possible.

## 2. Required Fields

Every `run_git_locked`-adjacent event must carry these fields:

| field | type | notes |
|---|---|---|
| `repo_slug` | string | Tokenized project or repository identifier. |
| `caller` | string | Static call-site name, populated by the wrapper or macro. |
| `args_hash` | string | SHA-256 of the joined argv. Raw argv requires `AM_LOG_GIT_ARGS=1`. |
| `duration_ms` | number | End-to-end duration in milliseconds. |
| `outcome` | enum | One of `success`, `retrying`, `succeeded_after_retry`, `exhausted`, `error`. |
| `git_version` | string | Cached resolved git version, or `unknown`. |

The current drift checker also supports `x-drift-required-fields` inside each
schema. That list is a tactical bridge for pre-existing events that have not
yet been fully upgraded to the baseline field set. The JSON Schemas below
describe the target contract; the drift checker enforces the currently adopted
subset so dashboards can converge without a single massive logging rewrite.

## 3. Metrics Catalog

| metric | type | labels | added in | description |
|---|---|---|---|---|
| `git_repo_lock_acquisitions_total` | counter | `repo_slug` | B2 | Per-process mutex acquisitions. |
| `git_repo_lock_wait_seconds` | hist | `repo_slug` | B2 | Buckets `[1ms, 10ms, 100ms, 1s]`. |
| `git_flock_wait_seconds` | hist | `repo_slug` | B3 | OS flock acquisition wait. |
| `git_flock_timeout_total` | counter | `repo_slug` | B3 | Flock timeouts. |
| `git_flock_readonly_total` | counter | `repo_slug` | B3 | Downgrade-to-read events. |
| `run_git_locked_total` | counter | `caller`,`outcome` | B4 | Every shell-out git call. |
| `run_git_locked_duration_seconds` | hist | `caller` | B4 | End-to-end runtime. |
| `run_git_locked_in_flight` | gauge | `caller` | B4 | Concurrent wrapped git invocations. |
| `run_git_locked_contention_seconds` | hist | `caller` | B4 | Wait on inner mutex plus flock. |
| `git_segfault_total` | counter | `repo_slug`,`caller` | E1/E3 | Observed segfault-like exits. |
| `git_segfault_retry_attempted_total` | counter | `caller` | E2 | First retry initiated. |
| `git_segfault_retry_succeeded_total` | counter | `caller`,`attempt_n` | E2 | Retry recovered. |
| `git_segfault_retry_exhausted_total` | counter | `caller` | E2 | Retries gave up. |
| `git_segfault_retry_latency_seconds` | hist | `caller`,`outcome` | E2 | Retry round-trip. |
| `reservation_activity_duration_seconds` | hist | `repo_slug` | C2 | libgit2 reservation walk. |
| `pathspec_walk_duration_seconds` | hist | `repo_slug` | C3 | Pathspec compile plus walk. |
| `doctor_fix_orphan_refs_actions_total` | counter | `op`,`outcome` | F1/F3 | Manual operator action. |

## 4. Schemas Index

Schemas live under `docs/schemas/git_251/`. Every schema uses JSON Schema
Draft 2020-12, has a stable `$id` under
`https://schemas.mcp-agent-mail/git_251/`, sets
`additionalProperties: false`, and declares top-level
`required: ["ts","level","target","name","fields"]`.

| schema | covers |
|---|---|
| `run_git_locked.schema.json` | `run_git_locked_*`, `git_locked_exit_*`, `git_spawn_failed`, `flock_acquire_failed`, and `git_binary_*` resolution/catalog events |
| `git_segfault_retry.schema.json` | `git_segfault_retry_*`, `guard_git_segfault_retry*` |
| `git_repo_lock.schema.json` | Per-process repo mutex events and metrics. |
| `git_flock.schema.json` | `flock_*` events from cooperative repo flocking. |
| `reservation_activity.schema.json` | `reservation_activity_*` events. |
| `pathspec_walk.schema.json` | `pathspec_walk_*` events. |
| `doctor_fix_orphan_refs.schema.json` | `fix_orphan_refs_*`, `ref_pruned`, packed refs and backup events. |
| `health_sweep.schema.json` | F5 health sweep events that consume this contract. |
| `boot_check.schema.json` | F8 boot-time archive integrity events that consume this contract. |

Each schema enumerates:

- `x-event-names`: event names covered by that schema.
- `x-planned`: true when the schema is intentionally defined ahead of code.
- `x-drift-required-fields`: field subset enforced against current code by
  `scripts/check_observability_drift.py`.
- `properties.fields.properties`: the Section 2 baseline fields plus
  event-specific extras.

## 5. Versioning And Drift Detection

When an event or field changes:

1. Bump `spec_version` patch for compatible additions, minor for broader
   additions, and major for breaking renames or removals.
2. Add a `## Changelog` entry.
3. Add or update the matching schema in `docs/schemas/git_251/`.
4. Add positive and negative fixture samples under
   `tests/fixtures/observability/<event_name>/`.
5. Run the drift checker and schema tests.

Drift automation:

- `scripts/check_observability_drift.py` scans Rust code for
  `tracing::*!(target = "mcp_agent_mail::*", ...)` calls in the git 2.51
  target set covered by this spec.
- It extracts target, event name, field names, and source location.
- It cross-references emitted events against `x-event-names` from the schemas.
- It reports structured JSON findings in three categories:
  `code_ahead`, `doc_ahead`, and `field_mismatch`.
- It exits `0` when findings are empty and `1` when findings are present.

The checker may use `ast-grep` when available and falls back to text scanning.
The fallback is deliberately conservative: it is good enough for the current
macro shape and reports locations so humans can inspect uncertain cases.

## 6. Cross-References

- `br-fqs7t` - E4 TUI toast: consumes `git_segfault_retry` fields and displays
  `repo_slug`.
- `br-66evl` - F5 health sweep: consumes `health_sweep.schema.json`.
- `br-8lp54` - F8 boot integrity: consumes `boot_check.schema.json`.
- `br-kgck8` - G2 stress harness: asserts the metrics catalog is emitted by
  stress scenarios.
- `br-q8yaa` - archive performance artifacts: reuses the same structured-log
  schema conventions.

## Changelog

- `1.0.0` - Initial E6 observability contract landed via `br-j1qwr`.
