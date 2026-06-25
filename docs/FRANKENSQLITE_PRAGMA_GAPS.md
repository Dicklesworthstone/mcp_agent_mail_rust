# FrankenSQLite ↔ Canonical SQLite Pragma Gap Report (M3 / br-bvq1x.13.3)

> **Living gap list.** `am doctor` runs its diagnostic pragmas through
> `DbConn = sqlmodel_frankensqlite::FrankenConnection`. Where FrankenSQLite's
> pragma surface *diverges* from canonical SQLite, the doctor must not treat the
> FrankenSQLite verdict as authoritative — it cross-checks against canonical
> (Track A3 double-probe) and/or falls back to canonical for the specific op
> (Track M2 capability table). This document tracks every known divergence, its
> reproducer, the compensating mechanism in `am`, and the **retirement checklist**
> for when FrankenSQLite conforms upstream.
>
> **Constraint (AGENTS.md / memory):** FrankenSQLite is an external sibling repo.
> This bead is **documentation + coordination only** — we never edit
> `/dp/frankensqlite`. Upstream fixes are requested, not made here.

## How to regenerate the underlying data

The machine-readable conformance matrix is produced by the **M1** harness, which
runs identical probes through canonical SQLite and FrankenSQLite against the same
fixtures (the L1 corpus plus a no-FK bootstrap DB) and diffs the verdicts:

```bash
cargo test -p mcp-agent-mail-db --test frankensqlite_pragma_conformance -- --nocapture
# report written to $CARGO_TARGET_TMPDIR/frankensqlite_pragma_conformance_report.json
```

Source of truth: `crates/mcp-agent-mail-db/tests/frankensqlite_pragma_conformance.rs`.
Each `(fixture, pragma)` pair records the canonical outcome, the FrankenSQLite
outcome, and a verdict of `Conformant` / `Divergent` / `Unsupported`.

## Current snapshot (2026-06-25)

| Metric | Value |
|--------|-------|
| Probes (10 pragmas × 12 fixtures) | 120 |
| Conformant | 85 |
| **Divergent** | **35** |
| Unsupported | 0 |

FrankenSQLite **implements every pragma `am` probes** (0 unsupported); all
divergences are *output* differences, not missing features. The 35 divergences
collapse into the two gaps below.

Pragmas probed: `quick_check`, `integrity_check`, `foreign_key_check`,
`foreign_key_list(messages)`, `foreign_key_list(child)`, `journal_mode`,
`wal_autocheckpoint`, `user_version`, `schema_version`, legacy FTS metadata.

---

## GAP-1 — `PRAGMA foreign_key_list` row ordering / `id` assignment divergence

- **Status:** DIVERGENT (11 probe entries: `foreign_key_list(messages)` ×8,
  `foreign_key_list(child)` ×3). Present on healthy DBs.
- **Incident anchor:** the ts2 "malformed-DB saga" — FrankenSQLite's foreign-key
  surface was the first place its pragma output was observed to differ from
  canonical (see also the now-fixed false-malformed `foreign_key_check`, GAP-2).

**Behavior.** Canonical and FrankenSQLite return the *same* foreign-key
relationships, but enumerate them in a **different order** and therefore assign
different `id=` values. Example (fixture `zero_byte_wal`, `PRAGMA foreign_key_list(messages)`):

```
canonical:      [id=0 table=agents  from=sender_id ...], [id=1 table=projects from=project_id ...]
frankensqlite:  [id=0 table=projects from=project_id ...], [id=1 table=agents  from=sender_id ...]
```

After sorting away the `id=`/`seq=` fields the row sets are identical — this is a
pure ordering/id-assignment divergence (the harness classifies it via
`is_fk_list_ordering_divergence`), not a missing or wrong foreign key.

**Reproducer.** Any DB with ≥2 foreign keys on a table; the harness uses the L1
fixtures. Run `PRAGMA foreign_key_list(messages)` through both engines and compare
the `id=` column.

**Impact on `am`.** **None today.** `am doctor` does not depend on canonical
`foreign_key_list` *ordering* or *id* values for any verdict — it consumes
`foreign_key_check` (violation presence) and the orphaned-recipient query, not
`foreign_key_list` enumeration order. There is **no M2 fallback to retire** for
this gap; it is tracked because any *future* consumer that keys off canonical
`foreign_key_list` ids would silently break.

**Retirement.** Close when FrankenSQLite assigns `foreign_key_list` `id=` values
in canonical (definition) order. No `am` code change needed; flip the harness's
`foreign_key_list_*` expectation from DIVERGENT to CONFORMANT.

---

## GAP-2 — Corruption-classification error-text divergence on malformed DBs

- **Status:** DIVERGENT (24 probe entries: `quick_check`, `integrity_check`,
  `foreign_key_check`, `journal_mode`, `wal_autocheckpoint`, `user_version`,
  `schema_version`, legacy-FTS metadata — each diverging on the **same 3
  malformed fixtures**: `btree_page_type_zero`, `freelist_leaf_exceeds_db_size`,
  `short_read_fetching_page`).
- **Incident anchor:** local session `c488ee28…` — FrankenSQLite reported a
  *healthy* FK-free DB as "database disk image is malformed" because its
  corruption-classification path diverged from canonical. This is the **root
  cause of the entire Track A / Track M reliability program.**

**Behavior.** Once a DB page is malformed, the two engines disagree on the *error
text* (and which pragma trips first), so every subsequent pragma read on that DB
diverges. Example (fixture `btree_page_type_zero`, `PRAGMA integrity_check`):

```
canonical:      Query error: file is not a database
frankensqlite:  Connection error: database disk image is malformed:
                invalid database header: invalid payload fractions: max=0 min=0 leaf=0
```

Both engines *agree the DB is bad*, but with different wording and at a different
layer (canonical: query-time; FrankenSQLite: connection-time). The danger the
program addresses is the **inverse** case (FrankenSQLite calling a *healthy* DB
malformed), which is why `am` never treats a FrankenSQLite corruption verdict as
authoritative without a canonical agreement.

**Reproducer.** The 3 malformed L1 fixtures; run `PRAGMA integrity_check` /
`quick_check` through both engines and compare the error text.

**Impact on `am` & compensating mechanism.**
- **A1** (`mcp_agent_mail_db::classify_db_error_message`) normalizes *both*
  engines' wording into the typed `DbErrorClass` taxonomy so the divergent text
  doesn't leak into verdicts.
- **A3** (`doctor_canonical_double_probe`, `crates/mcp-agent-mail-cli/src/lib.rs`)
  re-runs the full canonical read-only battery before any corruption verdict can
  stand.
- **A4** (`doctor_enforce_corruption_verdict_authority`) forbids an authoritative
  "malformed" verdict unless the canonical cross-check agrees.
- **M2** (`DoctorDiagnosticCapability` table) routes the two ops `am` most needs
  to trust — `foreign_key_check` and `orphaned_recipient_query` — through a
  FrankenSQLite-primary / canonical-fallback path, marking the verdict
  `DiagnosticEngineLimitation` when FrankenSQLite cannot answer authoritatively.

**Retirement.** Close when FrankenSQLite's corruption classification matches
canonical's *typed* outcome (it need not match the exact string — A1 already
normalizes wording — but it must agree on *healthy vs corrupt* for every L1
fixture, with no false-malformed). At that point:

| M2 fallback (cli/lib.rs) | Retire when |
|--------------------------|-------------|
| `DOCTOR_FOREIGN_KEY_CHECK_CAPABILITY` | FrankenSQLite `foreign_key_check` agrees with canonical (healthy vs violation) on all L1 fixtures — already CONFORMANT on healthy fixtures; keep until the 3 malformed fixtures stop diverging at the corruption layer. |
| `DOCTOR_ORPHANED_RECIPIENT_QUERY_CAPABILITY` | FrankenSQLite executes the orphaned-recipient join without diverging on malformed/edge DBs. |
| A3 `doctor_canonical_double_probe` cross-check | **Do not retire** — defense-in-depth against the *inverse* (false-malformed) regression; keep even after GAP-2 closes. |

---

## Conformant surface (no action)

On healthy DBs, FrankenSQLite is **conformant** for `quick_check`,
`integrity_check`, `foreign_key_check`, `journal_mode`, `wal_autocheckpoint`,
`user_version`, `schema_version`, and legacy-FTS metadata (85/120 probe entries).
The divergences are confined to GAP-1 (FK-list ordering, healthy DBs) and GAP-2
(corruption-error text, malformed DBs).

## Upstream coordination

FrankenSQLite is owned by the same maintainer as this repo; fixes belong upstream
in `/dp/frankensqlite`, **not here**. Ready-to-file issue summaries:

1. **`PRAGMA foreign_key_list` id ordering** (GAP-1): "FrankenSQLite enumerates
   `foreign_key_list` rows in a different order than canonical SQLite, assigning
   different `id=` values for the same relationships. Reproducer:
   `mcp-agent-mail-db` test `frankensqlite_pragma_conformance`,
   `foreign_key_list_messages` probe. Requested: match canonical definition-order
   id assignment."
2. **Corruption-classification parity** (GAP-2): "FrankenSQLite's malformed-DB
   error reporting trips at connection-time with different wording/layer than
   canonical, and historically false-flagged healthy FK-free DBs as malformed.
   Reproducer: same harness, malformed L1 fixtures. Requested: classify
   healthy-vs-corrupt identically to canonical for every L1 fixture."

When an upstream fix lands, re-run the M1 harness, flip the affected entries to
CONFORMANT, and tick the corresponding retirement row above.
