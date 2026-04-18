# SPEC: ATC Privacy & Data-Minimization Policy

> Audited 2026-04-18. Covers all fields entering the ATC experience pipeline.

## Design Principle

ATC stores **metrics, not content**. Raw message bodies, subjects, file paths, and secrets
must never enter the experience ledger, feature vectors, or transparency cards.

## Field Classification

### Category A — Metadata (No Privacy Concern)

Timestamps, counters, flags, enum variants, quantized probabilities, loss values.
These reveal operational state but not content.

Examples: `experience_id`, `state`, `posterior`, `expected_loss`, `calibration_healthy`,
all `FeatureVector` fields (quantized basis points, capped counters).

### Category B — Derived / Hashed (Low Risk)

Values computed from content but not reversible. Includes `non_execution_reason`
(budget names and thresholds), `FeatureExtension` (i64 key-value pairs).

### Category C — Pseudonymous Identifiers (Medium Risk)

Agent names (`subject`, `most_connected_agent`), project slugs (`project_key`),
thread IDs, policy IDs. Pseudonymous but correlatable over time.

**Mitigation**: Agent names are ephemeral by design (GreenCastle, BlueLake).
Project keys may reveal organizational structure — consider hashing in future.

### Category D — Content Fields (High Risk — Must Not Store Raw Content)

These fields accept freeform strings or `serde_json::Value` and **could** leak
sensitive content if populated incorrectly:

| Field | Location | Current Risk | Mitigation |
|-------|----------|-------------|------------|
| `evidence_summary` | ExperienceRow | HIGH | Must be metric-level summary, never raw text |
| `outcome.label` | ExperienceOutcome | MEDIUM | Should use constrained vocabulary |
| `outcome.evidence` | ExperienceOutcome | HIGH | Unschematized JSON — validate at write time |
| `context` | ExperienceRow | HIGH | Audit metadata — must not contain bodies/paths |
| `evidence` | EvidenceLedgerEntry | CRITICAL | Unschematized JSON — validate at write time |
| `expected` / `actual` | EvidenceLedgerEntry | MEDIUM | Short-lived (30d) but validate |
| `rationale` | CardEvidence | MEDIUM | Summary-level only |

### Category E — Participation Features (Medium Risk)

`ParticipationSnapshot` tracks coordination patterns: `co_participant_count`,
`high_risk_pair_count`, `most_connected_agent`, `recipients` lists.

**Risk**: Enables social graph inference (who coordinates with whom).

**Mitigation**: Participation events are not archived to Git. Summaries use
counts, not names. The `most_connected_agent` field stores a name — consider
replacing with count-only in future.

### Category F — Timing Features (Low Risk)

`silence_secs`, `throughput_per_min`, `active_agent_count`. Bucketized and
capped, revealing operational tempo but not schedules.

## Data-Minimization Rules

1. **No Category D content in feature vectors.** FeatureVector uses only
   quantized numerics — this is already enforced by the type system.

2. **No raw message bodies or subjects** in `evidence_summary`, `context`,
   `outcome.label`, or `outcome.evidence`. Callers must use structured
   summaries (e.g., "agent_responded_within_window" not "RE: Auth refactor").

3. **No file paths** in experience rows. Reservation counts are tracked as
   `reservation_count` (u8) in FeatureVector — paths are never stored.

4. **No secrets** in any field. Message content may contain AWS keys, tokens,
   etc. — ATC must never copy this into the learning pipeline.

5. **Evidence ledger entries are ephemeral.** Dropped after 30 days,
   never archived to Git (GIT_ARCHIVE_DENYLIST).

6. **Raw experience rows are never archived to Git.** The denylist in
   `atc_retention.rs` enforces this for OpenExperienceRows,
   ResolvedExperienceRows, and ExperienceRollups.

7. **Transparency cards are batched into summaries.** Individual cards
   are not archived; only periodic audit summaries reach Git.

## Retention Summary

| Artifact | SQLite Retention | Git Archive | Notes |
|----------|-----------------|-------------|-------|
| Open experience rows | Until resolved (max 30d) | Never | Denylist enforced |
| Resolved experience rows | 365 days | Never | Denylist enforced |
| Experience rollups | 730 days | Never | Denylist enforced |
| Evidence ledger entries | 30 days | Never | BoundedDebugTail strategy |
| Transparency cards | 365 days | Never (folded into summaries) | Batched audit |
| Audit summaries | 730 days | Archived | Aggregated, no raw content |
| Feature vectors | With parent row | Never | Quantized only |

## Validation Contract

The `ExperienceRow::validate()` method checks structural invariants. Future
work should add content validation:

- Reject `evidence_summary` longer than 256 chars or containing common
  secret patterns (AWS key prefix, Bearer token, etc.)
- Constrain `outcome.label` to a known vocabulary enum
- Schema-validate `context` and `outcome.evidence` JSON against an allowlist

## Privacy Strengths (Existing)

1. FeatureVector is pure quantized numerics — no content leakage possible
2. Git archive denylist prevents raw ATC data from reaching auditable history
3. Agent names are structurally pseudonymous (adjective+noun)
4. Evidence ledger auto-purges after 30 days
5. Participation events track counts and pairs, not message content
6. File paths are never stored — only reservation counts
