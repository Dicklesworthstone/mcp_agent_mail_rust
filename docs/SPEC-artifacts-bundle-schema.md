# SPEC: Artifact Bundle Schema (E2E / PTY / Fault Suites)

Issue: `br-3vwi.10.18`

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

Additional suite-specific artifacts are allowed anywhere under the bundle root
(e.g. request/response transcripts, server logs, step logs, failure bundles).

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
    "transcript": { "summary": { "path": "transcript/summary.txt" } }
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
- `kind`: one of `metadata|metrics|diagnostics|trace|transcript|opaque`
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

## Validator Tooling

Implementation lives in `scripts/e2e_lib.sh`:

- `e2e_write_bundle_manifest [artifact_dir]`
- `e2e_validate_bundle_manifest [artifact_dir]`

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

Negative/compat tests are in `tests/e2e/test_artifacts_schema.sh`.
