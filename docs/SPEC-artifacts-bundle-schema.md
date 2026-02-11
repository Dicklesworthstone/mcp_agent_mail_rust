# SPEC: Artifact Bundle Schema (E2E / PTY / Fault Suites)

Issues: `br-3vwi.10.18`, `br-3vwi.10.10`

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
- `diagnostics/env_redacted.txt` (redacted environment snapshot)
- `diagnostics/tree.txt` (deterministic file tree with sizes)
- `trace/events.jsonl` (structured events; `trace-events.v1`)
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
- `diagnostics` (`diagnostics/env_redacted.txt`, `diagnostics/tree.txt`)
- `trace` (`trace/events.jsonl`)
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
- `trace-events.v1`
- `fixtures.v1`
- `repro.v1`
- `logs-index.v1`
- `screenshots-index.v1`
- (reserved for future) `step.v1`, `failure.v1`

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
    "diagnostics": {
      "env_redacted": { "path": "diagnostics/env_redacted.txt" },
      "tree": { "path": "diagnostics/tree.txt" }
    },
    "trace": { "events": { "path": "trace/events.jsonl", "schema": "trace-events.v1" } },
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
- `kind`: one of `metadata|metrics|diagnostics|trace|transcript|log|screenshot|fixture|replay|opaque`
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

### `trace/events.jsonl` (`trace-events.v1`)

NDJSON stream: each non-empty line is a JSON object:

- `schema_version` (int, must be `1`)
- `suite` (string)
- `run_timestamp` (string; equals bundle `timestamp`)
- `ts` (string; RFC3339 recommended)
- `kind` (string; includes at least `suite_start` and `suite_end`)
- `case` (string; empty when not in a case)
- `message` (string; may be empty)
- `counters.total|pass|fail|skip` (int)

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

- `e2e_write_bundle_manifest [artifact_dir]`
- `e2e_validate_bundle_manifest [artifact_dir]`
- `e2e_validate_bundle_tree [artifacts_root]`

All E2E suites that call `e2e_summary` automatically:

1. Write required typed artifacts
2. Write `bundle.json`
3. Validate the bundle and fail the suite on violations

Validator enforcement (non-exhaustive):

- Required typed artifacts exist and match their schemas.
- `bundle.json` file inventory entries point to real files and `bytes` match on disk.
- Any non-empty `*.json` artifacts are valid JSON.
- Any `*.jsonl` / `*.ndjson` artifacts are valid line-delimited JSON.
  - Empty/whitespace-only `*.json` payloads are permitted as "no body" transcripts.
- `logs/index.json` and `screenshots/index.json` entries must match `bundle.json`
  file inventory hashes and byte sizes.

CI enforcement:

- CI jobs run `e2e_validate_bundle_tree tests/artifacts` after suite execution.
- Any malformed or incomplete bundle fails the job before artifact upload.

Negative/compat tests are in `tests/e2e/test_artifacts_schema.sh`.
