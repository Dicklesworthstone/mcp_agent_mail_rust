# Reservation Regression Fixture Corpus

This corpus captures the reservation drift modes from `br-bvq1x.6.4` so the
Track F consumers can share one set of incident-derived inputs:

- `br-bvq1x.6.1`: reservation write chokepoint fixtures
- `br-bvq1x.6.2`: DB/archive parity checker fixtures
- `br-bvq1x.6.3`: idempotent release fixtures

The manifest is the source of truth. Each fixture records the drift mode, the
expected consumer signal, and the artifact recipes needed to reproduce the
state. SQL recipes are standalone seeds for the minimal reservation tables.
Archive recipes are canonical `id-<reservation-id>.json` files using the same
field names the storage and reconstruct paths read.

The `btree_page_2288_release_malformed` fixture is reservation-specific glue
around the lower-level corrupted-DB corpus from `br-bvq1x.12.1`. It anchors the
malformed-B-tree-during-release incident while reusing the existing
`btree_page_type_zero` page-corruption recipe.
