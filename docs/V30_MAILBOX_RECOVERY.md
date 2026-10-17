# Preparing a repair copy for the v30 short-record incident

Tracked by **br-2hpuk**, recorded in commit
`6323b6acada5dcbc62bae8c78275bc60301df283`.

The v30 `ALTER TABLE messages ADD COLUMN archive_metadata_json TEXT` leaves
pre-existing records physically shorter than the new table schema. Canonical
SQLite reads those records correctly, but the affected FrankenSQLite versions
can shift fields and make inbox/search results disappear. A successful canonical
`integrity_check` alone therefore does not establish runtime readability.

## Diagnose stored records without producing a repair output

```sh
python3 scripts/materialize_v30_mailbox.py --check /path/to/offline-v30-backup.sqlite3
```

This command copies the backup into a private temporary directory, checks
canonical integrity, and walks the physical messages b-tree and overflow chains.
It counts the serial-type fields actually stored in each record rather than
counting SQL result columns (which SQLite fills from the newer schema).

The JSON report gives total/short/full record counts, a field-count histogram,
and at most eight affected message IDs. No message bodies are reported. Exit
status **2** with `status: requires_materialization` means short records exist;
exit status **0** with `status: full_records` means this particular physical
layout issue was not found. Exit status **1** means inspection was refused or
failed. Argument-usage errors also use status 2, but emit no JSON report.

A full-record result is **not** a runtime-engine health certificate. The probe
is deliberately specific to the v30 schema. The offline-backup and no-sidecar
requirements below apply to inspection too.

## Copy-only recovery preparation

Use Python 3.10+ with SQLite 3.37+; no extra packages are required:

```sh
python3 scripts/materialize_v30_mailbox.py \
  /path/to/offline-v30-backup.sqlite3 \
  /path/to/new-repaired-copy.sqlite3 > /path/to/repair-report.json
```

The input **must be an offline, standalone, checkpointed backup**, not the live
mailbox pathname. The tool refuses inputs with WAL, SHM, journal, or FrankenSQLite
namespace companions, even empty ones. Do not delete those companions to make a
live database pass this check. Stop writers and produce a consistent standalone
backup using the existing supervised recovery procedure first.

The utility copies source bytes with read-only OS I/O; it never opens the source
through SQLite. Only the private copy is opened for writing. It validates the
v30 message schema and canonical integrity, then executes, in one transaction:

```sql
UPDATE messages SET archive_metadata_json = archive_metadata_json;
```

This preserves both NULL and existing non-NULL reply metadata while materializing
the missing physical fields. It compares streaming, type-sensitive hashes of
**every ordinary table**, row identities, schema definitions, and application
metadata before and after. Trigger-induced changes to any table cause rollback
and refusal. Virtual/shadow tables, generated columns, and ambiguous schemas
are rejected rather than silently excluded from verification.

A second physical scan after commit must find the same message count and **zero
short records**. This catches an engine eliding the self-assignment UPDATE even
when every SQL-level equality and integrity check passes. Both physical-layout
reports are included in the repair report. The scanner handles multi-level table
b-trees, overflow-spanning headers, 512–65536-byte pages, and signed 64-bit rowids;
it rejects invalid pointers, cycles, truncated records, and inconsistent counts.

A successful result is published atomically without overwriting an existing
file. It is a standalone DELETE-journal database with private file permissions.
The JSON report contains source/output SHA-256 witnesses, table counts and
logical hashes, but no message bodies. Exit status 1 means refusal; do not use
an output unless the command reports `verified_repair_copy` successfully.

## This does not authorize or perform live promotion

The tool does not stop/restart a daemon, replace the live mailbox, remove
sidecars, change the migration ledger, or upgrade v29 databases. Validate the
new copy with the intended runtime engine and inspect known message IDs,
recipient inboxes, and search results before a separately supervised promotion.
Keep the original backup and report.

**br-2hpuk remains a release blocker:** the automatic materializing follow-up
migration, strict read-only robot admission with no migrating fallback, and the
upstream short-record decoder fix still need qualification. This utility is a
recovery-preparation capability, not a claim that those runtime fixes shipped.

## Regression tests

```sh
python3 -m unittest discover -s scripts/tests -p test_materialize_v30_mailbox.py -v
```

The tests independently read SQLite record headers to demonstrate that genuine
pre-ALTER 12-field records become 13-field records. They also exercise mixed old
and new rows, preservation of reply metadata and recipients, source byte
preservation, trigger rollback, sidecar rejection, and publication races.
Larger cases cover a 1,200-message multi-level b-tree with overflow payloads,
64 KiB pages, negative/nine-byte rowids, canonical-valid split record headers,
corrupt b-tree and overflow cycles, diagnostic exit codes, and failed physical
postconditions that must never publish a repair.
