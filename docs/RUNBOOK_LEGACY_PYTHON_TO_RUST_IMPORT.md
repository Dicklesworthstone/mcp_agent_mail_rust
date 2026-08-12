# Runbook: Legacy Python -> Rust Import (`am legacy`, `am upgrade`)

Last updated: 2026-02-17

## Goal

Migrate an existing Python-era Agent Mail installation (SQLite DB + Git-backed storage root) to the Rust schema and tooling without mutating the source, with an auditable receipt.

## Command Surface

- `am legacy detect`
- `am legacy import`
- `am legacy status`
- `am upgrade`

## Safety Model

1. `am legacy import` resolves DB/storage paths using precedence rules.
2. It opens the source only through canonical SQLite read-only access.
3. It creates a new target DB with SQLite's online backup API, copies the source storage root, and migrates only those targets.
4. It verifies canonical readability of both source and target, then runs target `PRAGMA integrity_check` and core-table count checks.
5. It writes a JSON receipt under `<target_storage_root>/legacy_import_receipts/`.
6. It runs setup refresh (`am setup run --yes`) and records whether refresh succeeded.

## Path Resolution Precedence

For DB and storage root independently:

1. Explicit CLI flags (`--db`, `--storage-root`)
2. Process environment (`DATABASE_URL`, `STORAGE_ROOT`)
3. Project `.env` under `--search-root`
4. User env file (`~/.mcp_agent_mail/.env`, then `~/mcp_agent_mail/.env`)
5. Legacy defaults

## Typical Workflows

### 1) Detect only

```bash
am legacy detect --search-root /abs/path/to/project
am legacy detect --search-root /abs/path/to/project --json
```

### 2) Dry-run import plan (no writes)

```bash
am legacy import --auto --search-root /abs/path/to/project --dry-run --yes
```

### 3) Copy-only import (default)

```bash
am legacy import --auto --search-root /abs/path/to/project --yes
```

### 4) Explicit source + target paths

```bash
am legacy import \
  --db /abs/path/to/storage.sqlite3 \
  --storage-root /abs/path/to/.mcp_agent_mail_git_mailbox_repo \
  --target-db /abs/path/to/storage.rust.sqlite3 \
  --target-storage-root /abs/path/to/.mcp_agent_mail_git_mailbox_repo-rust \
  --yes
```

### 5) One-command orchestrated upgrade

```bash
am upgrade --search-root /abs/path/to/project --yes
```

Behavior:

1. Detect legacy installation markers.
2. If found, run legacy import (auto path resolution).
3. If not found, run setup refresh only.

### 6) View import history

```bash
am legacy status --search-root /abs/path/to/project
am legacy status --search-root /abs/path/to/project --json
```

## Rollback Procedure

The source database and storage root are never changed by import. If target validation fails, retain the target paths for inspection and retry with new target paths after resolving the reported failure.

1. Find the latest receipt.

```bash
am legacy status --search-root /abs/path/to/project --json
```

2. Validate the source directly; it remains the authoritative untouched store.

```bash
am doctor check --json
```

## Operational Checks After Successful Import

1. `am legacy status --json` shows the new receipt.
2. `integrity_check_ok` is `true` in receipt.
3. `setup_refresh_ok` is `true` (or warning documented if false).
4. Core table counts are non-zero where expected for active installs.
5. Agent workflow sanity smoke test succeeds:

```bash
am mail status /abs/path/to/project
```

## Troubleshooting

### `no legacy installation detected`

Run with JSON output and inspect marker details:

```bash
am legacy detect --search-root /abs/path --json
```

Then retry with explicit DB/storage flags.

### `target storage root already exists and is not empty`

Use a fresh target path. Legacy import never overwrites or migrates the source in place.

### `integrity_check failed after migration`

Do not continue; inspect the target failure and confirm the source remains readable before retrying with fresh target paths.

### setup refresh warning

Import may still be successful. Retry setup explicitly:

```bash
am setup run --yes --project-dir /abs/path/to/project
```
