# SPEC: ATC Core Contract (Sealed)

> Sealed 2026-04-18. Schema v16 is expected to match these shapes and invariants exactly.

## Scope

This spec covers the foundational ATC (Air Traffic Controller) contract consumed by the downstream learning loop: experience capture, evidence emission, baseline calibration, and operator-configurable runtime thresholds. The goal of this document is to seal the v1 contract, not to describe every downstream consumer.

## Sealed Contract

### `ExperienceRow` (`experience.rs`)

`ExperienceRow` is the persisted decision record for a single ATC intervention candidate.

Sealed invariants:

- The lifecycle is an 11-state directed automaton: `Planned -> Dispatched -> {Executed, Failed, Throttled, Suppressed, Skipped}` and `Executed -> Open -> {Resolved, Censored, Expired}`.
- `validate_transition()` is the authoritative transition gate. Same-state transitions remain idempotent.
- `validate()` is the authoritative structural checker for state-dependent required fields.
- Terminal transitions are idempotent: resolving, censoring, or expiring an already terminal row is a no-op instead of a duplicate state mutation.
- Serde round-trips must preserve every field without introducing structural invalidity.

Guard rails in code:

- Property test: `experience_row_roundtrip_property`
- Property test: `validate_transition_matches_contract`
- Unit tests covering idempotent resolution and state-specific validation failures

### `FeatureVector` (`experience.rs`)

`FeatureVector` is a fixed-layout compact feature payload with `FEATURE_VERSION = 1`. Numerical fields are quantized to basis points to keep the persisted representation stable and bounded. Any semantic change to an existing feature field requires a `FEATURE_VERSION` bump and downstream audit.

### `ExperienceOutcome` (`experience.rs`)

`ExperienceOutcome` carries the observed result for a completed experience: observed timestamp, label, correctness, realized loss, regret, and optional supporting evidence payload. Resolved rows require an outcome.

### `EvidenceLedgerEntry` and `EvidenceLedger` (`evidence_ledger.rs`)

`EvidenceLedgerEntry` is the append-only evidence envelope for explainable runtime decisions. `EvidenceLedger` is the in-memory ring buffer plus optional JSONL sink.

Sealed invariants:

- Sequence numbers are monotonic within a ledger instance.
- Decision records are append-only.
- `record_outcome()` backfills the in-memory entry and appends a sideband JSONL outcome line instead of mutating an earlier record already written to disk.
- Serializing the same `EvidenceLedgerEntry` twice must produce the same JSON bytes under the current serializer configuration.
- Serde round-trips must preserve the full entry payload.

Guard rails in code:

- Property test: `entry_json_roundtrip_is_stable`
- Unit tests covering bounded retention, hit-rate computation, query filtering, JSONL output, and monotonic `seq`

### `atc_baseline.rs`

`atc_baseline.rs` is a real captured pre-learning baseline, not a placeholder. It seals the default timing budgets, calibration thresholds, loss matrices, priors, adaptive controller thresholds, program priors, and the documented operator/diagnostic gaps that explain current observability limits.

For the core contract, the important guarantee is that config defaults are expected to align with the baseline timing and calibration constants where the runtime exposes corresponding operator knobs.

### Config Surface (`config.rs`)

The sealed ATC environment surface is:

- `AM_ATC_ENABLED`
- `AM_ATC_PROBE_INTERVAL_SECS`
- `AM_ATC_ADVISORY_COOLDOWN_SECS`
- `AM_ATC_SUMMARY_INTERVAL_SECS`
- `AM_ATC_SAFE_MODE_RECOVERY_COUNT`
- `AM_ATC_EPROCESS_THRESHOLD`
- `AM_ATC_CUSUM_THRESHOLD`
- `AM_ATC_CUSUM_DELTA`
- `AM_ATC_LEDGER_CAPACITY`
- `AM_ATC_SUSPICION_K`

Sealed invariants:

- `Config::default()` aligns with `BASELINE_TIMING`, `BASELINE_CALIBRATION`, and `BASELINE_SUSPICION_K`.
- Interval and capacity knobs clamp to safe lower bounds during env parsing.
- Invalid non-positive or non-finite float overrides do not replace the defaults.

Guard rails in code:

- Unit test: `test_atc_defaults_align_with_baseline_contract`
- Unit test: `test_atc_env_overrides_and_clamps`

## Known Non-Goals

- The evidence ledger ring buffer is not itself serialized; JSONL is the recovery format by design.
- Operator visibility gaps and diagnostic blind spots remain documented baseline facts, not schema bugs.
- This spec does not seal downstream learning-policy logic, only the core input/output contract those components depend on.

## Evolution Rules

1. Changing the meaning of an existing `ExperienceRow`, `ExperienceOutcome`, or `EvidenceLedgerEntry` field requires updating this spec and auditing all downstream ATC consumers.
2. Adding or removing persisted fields requires a schema migration decision before implementation.
3. Changing the meaning or scale of an existing `FeatureVector` field requires a `FEATURE_VERSION` bump.
4. Adding a new `ExperienceState` requires updating both `validate_transition()` and `validate()`, plus their contract tests.
5. Changing baseline timing or calibration constants requires re-validating the config defaults that mirror them.
