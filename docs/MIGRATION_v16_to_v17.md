# ATC Migration: v16 -> v17 Surface

Tracks the additive ATC schema surface introduced for `br-bn0vb.28`.

## What Changed

The ATC migration set now reserves schema for the next ATC seams:

- `atc_leader_lease` table for DB-backed leader election state
- `atc_experiences.contained_suspected_secret` with default `0`
- `atc_experiences.privacy_classification` with default `legacy_unclassified`
- `atc_rollup_snapshots` table plus capture-time index

These migrations are implemented inline in
[`crates/mcp-agent-mail-db/src/schema.rs`](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-db/src/schema.rs)
because this repo does not use per-version migration files.

## Migration IDs

The new additive migrations are:

- `v17_create_atc_leader_lease`
- `v17_atc_experiences_add_contained_suspected_secret`
- `v17_atc_experiences_add_privacy_classification`
- `v17_create_atc_rollup_snapshots`
- `v17_idx_atc_rollup_snapshots_captured`

## Operator Notes

- The migration is additive and idempotent. No tables or columns are dropped.
- Existing `atc_experiences` rows receive:
  - `contained_suspected_secret = 0`
  - `privacy_classification = 'legacy_unclassified'`
- There is no down-migration. Downgrading would require manual destructive cleanup
  of the new ATC tables/columns and is intentionally unsupported.

## Verification

The migration/test coverage for this surface lives in:

- [`crates/mcp-agent-mail-db/tests/schema_migration.rs`](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-db/tests/schema_migration.rs)
- [`crates/mcp-agent-mail-db/src/queries.rs`](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-db/src/queries.rs)

Verified lanes:

- fresh DB -> latest schema includes the ATC v17 surface
- pre-v17 seeded ATC row -> latest schema preserves the row and backfills defaults
- invalid `privacy_classification` values fail the DB constraint
- `reconstruct_from_archive` recreates the v17 schema surface

## Important Scope Boundary

This patch lands the schema surface and migration coverage only.

- `br-bn0vb.24` still owns leader-election behavior on top of `atc_leader_lease`
- `br-bn0vb.27` still owns snapshot serialization/restore behavior on top of `atc_rollup_snapshots`
- `br-bn0vb.22` owns the runtime producer logic that will populate the new privacy fields
