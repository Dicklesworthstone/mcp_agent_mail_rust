# SPEC: Artifact Bundle Schema (E2E / PTY / Fault Suites)

Issues: `br-3vwi.10.18`, `br-3vwi.10.10`, `br-oci92.12`

This repo produces forensic artifacts under `tests/artifacts/` for all V2 test
suites (bash E2E scripts, PTY interaction suites, fault-injection suites).
This spec defines a **versioned bundle contract** so regression triage can be
uniform and automatable.

## Goals

- Every suite emits a complete, versioned, machine-parseable artifact bundle.
- CI fails fast on schema drift, missing required artifacts, or malformed JSON.
- Schema evolution is explicit (major/minor) with forward-compatible additions.

## Bundle Layout

Bundle root directory:

`tests/artifacts/<suite>/<timestamp>/`

Required files (v1):

- `bundle.json` (bundle manifest; authoritative inventory + typed references)
- `summary.json` (aggregate counters; `summary.v1`)
- `meta.json` (run metadata; `meta.v1`)
- `metrics.json` (timing + counters; `metrics.v1`)
- `manifest.json` (case manifest; `e2e-manifest.v1`)
- `diagnostics/env_redacted.txt` (redacted environment snapshot)
- `diagnostics/tree.txt` (deterministic file tree with sizes)
- `trace/events.jsonl` (structured events; `trace-events.v2`)
- `scenarios/index.json` (scenario replay summary; `scenarios-index.v1`)
- `scenarios/scenarios.jsonl` (per-case replay records; `scenario-events.v1`)
- `transcript/summary.txt` (human-readable quick context)
- `repro.txt` (copy/paste replay command)
- `repro.env` (deterministic replay env vars)
- `repro.json` (machine-readable replay metadata; `repro.v1`)
- `fixtures.json` (fixture identifiers used by the run; `fixtures.v1`)
- `logs/index.json` (normalized inventory of all `*.log` artifacts; `logs-index.v1`)
- `screenshots/index.json` (normalized inventory of all screenshot artifacts; `screenshots-index.v1`)

Additional suite-specific artifacts are allowed anywhere under the bundle root
(e.g. request/response transcripts, server logs, step logs, failure bundles).

## Artifact Taxonomy (V2 Regression Forensics)

Every bundle must expose these sections in `bundle.json.artifacts`:

- `metadata` (`meta.json`)
- `metrics` (`metrics.json`)
- `summary` (`summary.json`)
- `manifest` (`manifest.json`)
- `diagnostics` (`diagnostics/env_redacted.txt`, `diagnostics/tree.txt`)
- `trace` (`trace/events.jsonl`)
- `scenarios` (`scenarios/index.json`, `scenarios/scenarios.jsonl`)
- `transcript` (`transcript/summary.txt`)
- `logs` (`logs/index.json`)
- `screenshots` (`screenshots/index.json`)
- `fixtures` (`fixtures.json`)
- `replay` (`repro.txt`, `repro.env`, `repro.json`)

Section intent:

- `logs`: canonical entrypoint for server/harness/runtime logs.
- `screenshots`: canonical entrypoint for visual diffs, snapshots, and captures.
- `fixtures`: stable identifiers/paths for test fixtures used in this run.
- `replay`: one-command deterministic replay metadata.
- `scenarios`: per-case replay records derived from the trace and manifest.

## Retention Policy (V2 Regressions)

CI upload retention is controlled by GitHub Actions `retention-days` and is set
to **14 days** for all bundled artifacts in this repository.

Policy:

- Default forensic bundles: keep for 14 days in CI artifacts.
- P0/P1 regressions requiring longer analysis: operators should re-upload or
  attach extracted bundles to a tracking issue before expiry.
- Local `tests/artifacts/**` directories are developer-managed and may exceed CI
  retention, but must still conform to this bundle schema.

## Versioning / Evolution Strategy

Bundle schema version is encoded in `bundle.json`:

- `schema.name` is fixed: `mcp-agent-mail-artifacts`
- `schema.major` is **breaking**:
  - removing/renaming required keys
  - changing required types/semantics
  - changing required file paths
- `schema.minor` is **additive**:
  - adding new optional keys
  - adding new optional artifact references
  - adding new file kinds/schemas

Compatibility rules:

1. Validators MUST accept any `schema.minor >= 0` for the current `schema.major`.
2. New fields added in a minor version must be optional in validators.
3. Deprecations must be staged:
   - mark optional first (minor)
   - remove only in next major

Per-file schema identifiers (strings used in `bundle.json`):

- `summary.v1`
- `meta.v1`
- `metrics.v1`
- `e2e-manifest.v1`
- `trace-events.v2`
- `scenarios-index.v1`
- `scenario-events.v1`
- `fixtures.v1`
- `repro.v1`
- `logs-index.v1`
- `screenshots-index.v1`
- `failure.v1`
- (reserved for future) `step.v1`

## `bundle.json` Schema (v1)

Top-level shape (required keys):

```json
{
  "schema": { "name": "mcp-agent-mail-artifacts", "major": 1, "minor": 0 },
  "suite": "dual_mode",
  "timestamp": "20260210_170000",
  "generated_at": "2026-02-10T17:00:05Z",
  "started_at": "2026-02-10T17:00:00Z",
  "ended_at": "2026-02-10T17:00:05Z",
  "counts": { "total": 42, "pass": 40, "fail": 2, "skip": 0 },
  "git": { "commit": "...", "branch": "main", "dirty": false },
  "artifacts": {
    "metadata": { "path": "meta.json", "schema": "meta.v1" },
    "metrics": { "path": "metrics.json", "schema": "metrics.v1" },
    "summary": { "path": "summary.json", "schema": "summary.v1" },
    "manifest": { "path": "manifest.json", "schema": "e2e-manifest.v1" },
    "diagnostics": {
      "env_redacted": { "path": "diagnostics/env_redacted.txt" },
      "tree": { "path": "diagnostics/tree.txt" }
    },
    "trace": { "events": { "path": "trace/events.jsonl", "schema": "trace-events.v2" } },
    "scenarios": {
      "index": { "path": "scenarios/index.json", "schema": "scenarios-index.v1" },
      "events": { "path": "scenarios/scenarios.jsonl", "schema": "scenario-events.v1" }
    },
    "transcript": { "summary": { "path": "transcript/summary.txt" } },
    "logs": { "index": { "path": "logs/index.json", "schema": "logs-index.v1" } },
    "screenshots": { "index": { "path": "screenshots/index.json", "schema": "screenshots-index.v1" } },
    "fixtures": { "path": "fixtures.json", "schema": "fixtures.v1" },
    "replay": {
      "command": { "path": "repro.txt" },
      "environment": { "path": "repro.env" },
      "metadata": { "path": "repro.json", "schema": "repro.v1" }
    }
  },
  "files": [
    { "path": "summary.json", "sha256": "…", "bytes": 123, "kind": "metrics", "schema": "summary.v1" }
  ]
}
```

File inventory entry schema:

- `path`: relative path under bundle root (no absolute paths; no `..`)
- `sha256`: 64 lowercase hex chars
- `bytes`: integer byte size
- `kind`: one of `metadata|metrics|diagnostics|trace|transcript|log|screenshot|fixture|replay|scenario|opaque`
- `schema`: string (known typed artifacts) or `null`

## Required Typed Artifact Schemas

### `summary.json` (`summary.v1`)

```json
{
  "schema_version": 1,
  "suite": "dual_mode",
  "timestamp": "20260210_170000",
  "started_at": "2026-02-10T17:00:00Z",
  "ended_at": "2026-02-10T17:00:05Z",
  "total": 42,
  "pass": 40,
  "fail": 2,
  "skip": 0
}
```

### `meta.json` (`meta.v1`)

Required keys:

- `schema_version` (int)
- `suite`, `timestamp`, `started_at`, `ended_at` (string)
- `git.commit`, `git.branch` (string), `git.dirty` (bool)
- `runner.*` and `paths.*` are informational but must remain objects

### `metrics.json` (`metrics.v1`)

Required keys:

- `schema_version` (int)
- `suite`, `timestamp` (string)
- `timing.start_epoch_s`, `timing.end_epoch_s`, `timing.duration_s` (int)
- `counts.total|pass|fail|skip` (int)

### `manifest.json` (`e2e-manifest.v1`)

Required keys:

- `schema_version` (int)
- `test_suite` (string; equals bundle `suite`)
- `started_at`, `finished_at` (string; equal summary start/end values)
- `cases` (array of objects)
  - `name` (string; unique within the manifest)
  - `status` (`pass|fail|skip|unknown`)
  - `duration_ms` (int, non-negative)
  - `assertion_count` (int, non-negative)
  - `artifacts` (array of paths that must exist in `bundle.json.files`)
- `environment`, `server_config` (objects)
- `rerun_command` (string)

### `trace/events.jsonl` (`trace-events.v2`)

NDJSON stream: each non-empty line is a JSON object:

- `schema_version` (int, must be `1` or `2`)
- `suite` (string)
- `run_timestamp` (string; equals bundle `timestamp`)
- `ts` (string; RFC3339 recommended)
- `kind` (string; includes at least `suite_start` and `suite_end`)
- `case` (string; empty when not in a case)
- `message` (string; may be empty)
- `counters.total|pass|fail|skip` (int)
- Optional v2 fields: `assertion_id` (string), `step` (string), `elapsed_ms` (number)

### Scenario Replay Artifacts (`scenarios-index.v1`, `scenario-events.v1`)

`scenarios/index.json` is the summary entrypoint:

- `schema_version` (int)
- `suite`, `timestamp` (string)
- `scenario_count` (int; equals the number of scenario JSONL records)
- `scenarios_jsonl` (`scenarios/scenarios.jsonl`)
- `generated_from.manifest` (`manifest.json`)
- `generated_from.trace` (`trace/events.jsonl`)
- `statuses` (object keyed by `pass|fail|skip|unknown`)
- `replay.command`, `replay.environment`, `replay.metadata`
- `diagnostics.env_redacted`

`scenarios/scenarios.jsonl` contains one record per manifest case:

- `schema_version` (int)
- `suite`, `run_timestamp` (string)
- `case` (string; must match a `manifest.json` case)
- `status` (`pass|fail|skip|unknown`)
- `duration_ms`, `assertion_count` (non-negative ints)
- `artifact_dir` (absolute artifact directory for local triage)
- `artifacts` (array of `{ "path": "..." }` references present in `bundle.json.files`)
- `trace.events` (`trace/events.jsonl`)
- `replay.command`, `replay.environment`, `replay.metadata`
- `diagnostics.env_redacted`
- `failures` (array; `e2e_diff` entries include `artifact`, `label`, `expected`,
  `actual`, `replay_command`, and `env_redacted_path`)

## Scenario Replay vs Direct Scripts

Scenario replay logs are a shared artifact layer, not a second test framework.
Use the existing direct E2E scripts for setup, transport selection, server
lifecycle, and assertions. Use scenario replay artifacts when a result needs
portable triage evidence across real stdio, HTTP, CLI, or UI routes:

- R0/R1 closure claims that need a stable per-case replay record.
- Failure triage that needs the command, redacted environment, trace, artifact
  directory, and expected-vs-actual diff in one machine-readable place.
- Cross-suite dashboards or release gates that need one scenario shape without
  parsing each suite's bespoke stdout.

Direct scripts remain the right surface for local debugging, new assertion
helpers, and suite-specific fixture setup. Suites that call `e2e_summary`
automatically emit scenario replay logs, so new scripts should use the standard
summary path instead of writing custom replay JSON.

### `fixtures.json` (`fixtures.v1`)

Required keys:

- `schema_version` (int)
- `suite`, `timestamp` (string)
- `fixture_ids` (array of unique strings; may be empty)

### `repro.json` (`repro.v1`)

Required keys:

- `schema_version` (int)
- `suite`, `timestamp` (string)
- `clock_mode` (string)
- `seed` (int)
- `run_started_at` (string)
- `run_start_epoch_s` (int)
- `command` (string)

### `logs/index.json` (`logs-index.v1`)

Required keys:

- `schema_version` (int)
- `suite`, `timestamp` (string)
- `files` (array of objects)
  - `path` (relative path under bundle root)
  - `bytes` (int)
  - `sha256` (64 lowercase hex string)

### `screenshots/index.json` (`screenshots-index.v1`)

Required keys:

- `schema_version` (int)
- `suite`, `timestamp` (string)
- `files` (array of objects)
  - `path` (relative path under bundle root)
  - `bytes` (int)
  - `sha256` (64 lowercase hex string)

## Validator Tooling

Implementation lives in `scripts/e2e_lib.sh`:

- `e2e_write_suite_manifest_json [artifact_dir]`
- `e2e_write_scenario_logs [artifact_dir]`
- `e2e_write_bundle_manifest [artifact_dir]`
- `e2e_validate_bundle_manifest [artifact_dir]`
- `e2e_validate_bundle_tree [artifacts_root]`

All E2E suites that call `e2e_summary` automatically:

1. Write required typed artifacts
2. Write `manifest.json`
3. Write `scenarios/index.json` and `scenarios/scenarios.jsonl`
4. Write `bundle.json`
5. Validate the bundle and fail the suite on violations

Validator enforcement (non-exhaustive):

- Required typed artifacts exist and match their schemas.
- `bundle.json` file inventory entries point to real files and `bytes` match on disk.
- Any non-empty `*.json` artifacts are valid JSON.
- Any `*.jsonl` / `*.ndjson` artifacts are valid line-delimited JSON.
  - Empty/whitespace-only `*.json` payloads are permitted as "no body" transcripts.
- `logs/index.json` and `screenshots/index.json` entries must match `bundle.json`
  file inventory hashes and byte sizes.
- `scenarios/index.json` and `scenarios/scenarios.jsonl` must match `manifest.json`
  cases and every referenced replay, diagnostic, failure, and case artifact must
  exist in `bundle.json.files`.

CI enforcement:

- CI jobs run `e2e_validate_bundle_tree tests/artifacts` after suite execution.
- Any malformed or incomplete bundle fails the job before artifact upload.

Negative/compat tests are in `tests/e2e/test_artifacts_schema.sh`.
