# Preparing a repair copy for the v30 short-record incident

Tracked by **br-2hpuk**, recorded in commit
`6323b6acada5dcbc62bae8c78275bc60301df283`.

The v30 `ALTER TABLE messages ADD COLUMN archive_metadata_json TEXT` leaves
pre-existing records physically shorter than the new table schema. Canonical
SQLite reads those records correctly, but the affected FrankenSQLite versions
can shift fields and make inbox/search results disappear. A successful canonical
`integrity_check` alone therefore does not establish runtime readability.

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
