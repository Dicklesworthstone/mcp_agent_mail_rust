# SPEC: ATC Core Contract (Sealed)

> Sealed 2026-04-18. Schema v16 matches these shapes. Changes require migration.

## Scope

This spec covers the foundational data types that flow through the ATC (Air Traffic Controller) learning pipeline. All downstream modules — labeling, attribution, fairness, contamination, risk budgets, policy certificates — consume these shapes. They are frozen: field additions require a version bump, removals require a migration, and semantic changes require audit.

## Sealed Types

### `ExperienceRow` (experience.rs)

38-field struct representing one ATC decision and its outcome. Key invariants:

- **Lifecycle**: 11 states forming a directed acyclic automaton (Planned → Dispatched → Executed/Failed/Throttled/Suppressed/Skipped; Executed → Open → Resolved/Censored/Expired).
- **Idempotent resolution**: resolving/censoring/expiring an already-terminal row is a no-op.
- **Validation**: `validate()` checks state-to-required-fields mapping (e.g., Resolved requires outcome, Skipped requires non_execution_reason).
- **Serialization**: all fields serialize/deserialize via serde. Optional fields use `skip_serializing_if`.

### `FeatureVector` (experience.rs)

Fixed 64-byte compact feature vector. FEATURE_VERSION = 1. Quantized to basis points (0–10000) for numerical stability. Includes posterior probabilities, silence duration, reservation/conflict counts, throughput, inbox depth, expected loss, calibration flags, controller mode, and risk tier.

### `ExperienceOutcome` (experience.rs)

6-field struct: observed_ts_micros, label, correct, actual_loss, regret, evidence.

### `EvidenceLedgerEntry` (evidence_ledger.rs)

11-field struct with monotonic seq, decision_id, action, confidence, expected_loss, expected/actual outcomes, correct flag, trace_id, model. Serialized as JSONL (deterministic, append-only).

### `EvidenceLedger` (evidence_ledger.rs)

In-memory ring buffer with JSONL file output. Monotonic sequence numbers via AtomicU64. Thread-safe write lock. Supports outcome backfill via `record_outcome(seq, actual, correct)`.

### `atc_baseline.rs`

Frozen pre-learning baseline constants captured 2026-03-18:
- Loss matrices for Liveness (3x3), Conflict (3x3), LoadRouting (3x3)
- Prior probability vectors (sum to 1.0)
- Timing budgets (tick=5ms, probe=120s, advisory_cooldown=300s)
- Calibration thresholds (eprocess=20.0, cusum=5.0)
- Adaptive controller params (pressure=0.75, conservative=0.90)
- Program priors (claude-code=60s, codex/gemini/copilot=120s, unknown=300s)
- 10 documented operator visibility gaps, 11 diagnostic blind spots

### Config Surface (config.rs)

10 environment variables: `AM_ATC_ENABLED`, `AM_ATC_PROBE_INTERVAL_SECS`, `AM_ATC_ADVISORY_COOLDOWN_SECS`, `AM_ATC_SUMMARY_INTERVAL_SECS`, `AM_ATC_SAFE_MODE_RECOVERY_COUNT`, `AM_ATC_EPROCESS_THRESHOLD`, `AM_ATC_CUSUM_THRESHOLD`, `AM_ATC_CUSUM_DELTA`, `AM_ATC_LEDGER_CAPACITY`, `AM_ATC_SUSPICION_K`. All bounds-clamped with sensible defaults aligned to baseline.

## Test Coverage

184+ tests across core contract files:
- 32+ in experience.rs (lifecycle, idempotency, serde, feature vector, validation, sealed contract roundtrips)
- 20 in evidence_ledger.rs (ring buffer, outcomes, hit rate, JSONL I/O)
- 11 in atc_baseline.rs (asymmetry, priors, timing, calibration, serde)
- 121 in config.rs (parsing, bounds, serde)

## Known Gaps (Documented, Not Blocking)

- Evidence ledger in-memory state is not serializable (JSONL is the recovery format — by design).
- 10 operator visibility gaps documented in `BASELINE_OPERATOR_GAPS`.
- 11 diagnostic blind spots documented in `BASELINE_DIAGNOSTIC_GAPS`.
- Feature vector evolution relies on FEATURE_VERSION field (no automated migration).

## Evolution Rules

1. Adding optional trailing fields to ExperienceRow: OK without version bump.
2. Changing meaning/scale of existing FeatureVector fields: bump FEATURE_VERSION.
3. Removing a field from ExperienceRow: requires DB migration + FEATURE_VERSION bump.
4. Adding a new ExperienceState variant: requires updating validate() and all downstream state-machine consumers.
5. Changing loss matrix dimensions: requires atc_baseline.rs update + downstream revalidation.
