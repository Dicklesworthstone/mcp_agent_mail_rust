# SPEC: ATC Data-Minimization — Field-Level Classification

> Audited 2026-04-18. Covers every field in `ExperienceRow`, `FeatureVector`,
> `FeatureExtension`, `ExperienceOutcome`, and `NonExecutionReason`.

## Principle

ATC stores **metrics, not content**. The type system enforces this for the
hot path (FeatureVector is pure quantized numerics). Freeform fields are
validated at write time by `ExperienceRow::validate()` and `contains_secret()`.

## Privacy Categories

| Cat | Name | Risk | Description |
|-----|------|------|-------------|
| A | Metadata | None | Timestamps, counters, flags, enum variants, quantized values |
| B | Derived/Hashed | Low | Computed from content, not reversible |
| C | Pseudonymous ID | Medium | Correlatable over time but not personally identifying |
| D | Content | **High** | Freeform strings that could leak sensitive data |
| E | Participation | Medium | Coordination patterns enabling social graph inference |
| F | Timing | Low | Bucketized operational tempo metrics |

---

## ExperienceRow Field Classification

| Field | Type | Cat | Rationale |
|-------|------|-----|-----------|
| `experience_id` | u64 | A | Monotonic counter |
| `decision_id` | u64 | A | Monotonic counter |
| `effect_id` | u64 | A | Monotonic counter |
| `trace_id` | String | B | FNV-1a hash of decision context |
| `claim_id` | String | B | Artifact-graph reference (opaque ID) |
| `evidence_id` | String | B | Artifact-graph reference (opaque ID) |
| `state` | enum | A | Lifecycle state (11 fixed variants) |
| `subsystem` | enum | A | ATC subsystem (5 fixed variants) |
| `decision_class` | String | B | Constrained vocabulary (e.g. "liveness.check") |
| `subject` | String | **C** | Agent name (pseudonymous: adjective+noun) |
| `project_key` | Option\<String\> | **C** | Project path — may reveal org structure |
| `policy_id` | Option\<String\> | B | Policy artifact reference |
| `effect_kind` | enum | A | Effect taxonomy (7 fixed variants) |
| `action` | String | B | Action label from constrained vocabulary |
| `posterior` | Vec\<(String, f64)\> | A | State labels are constrained; probabilities are numeric |
| `expected_loss` | f64 | A | Numeric metric |
| `runner_up_action` | Option\<String\> | B | Action label from constrained vocabulary |
| `runner_up_loss` | Option\<f64\> | A | Numeric metric |
| `evidence_summary` | String | **D** | Freeform — validated by `contains_secret()`, max 256 chars |
| `calibration_healthy` | bool | A | Flag |
| `safe_mode_active` | bool | A | Flag |
| `non_execution_reason` | Option\<enum\> | B | Structured enum with numeric fields |
| `outcome` | Option\<struct\> | **D** | Contains freeform `label` and unschematized `evidence` JSON |
| `created_ts_micros` | i64 | A | Timestamp |
| `dispatched_ts_micros` | Option\<i64\> | A | Timestamp |
| `executed_ts_micros` | Option\<i64\> | A | Timestamp |
| `resolved_ts_micros` | Option\<i64\> | A | Timestamp |
| `features` | Option\<FeatureVector\> | A | Pure quantized numerics (see below) |
| `feature_ext` | Option\<FeatureExtension\> | B | Key-value i64 pairs (keys are constrained) |
| `context` | Option\<Value\> | **D** | Unschematized JSON — validated by `contains_secret()` |

## FeatureVector Field Classification

Every field is **Category A (Metadata)** — quantized numerics with no content.

| Field | Type | Range | Subcategory |
|-------|------|-------|-------------|
| `version` | u16 | Fixed | Schema metadata |
| `posterior_alive_bp` | u16 | 0..10000 | Liveness — quantized probability |
| `posterior_flaky_bp` | u16 | 0..10000 | Liveness — quantized probability |
| `silence_secs` | u16 | 0..65535 | Timing (F) — capped at 18.2h |
| `observation_count` | u16 | 0..65535 | Counter |
| `reservation_count` | u8 | 0..255 | Conflict — count only, no file paths |
| `conflict_count` | u8 | 0..255 | Conflict — count only |
| `in_deadlock_cycle` | bool | 0/1 | Conflict — flag |
| `throughput_per_min` | u8 | 0..255 | Timing (F) — capped |
| `inbox_depth` | u8 | 0..255 | Counter |
| `expected_loss_bp` | u16 | 0..10000 | Decision quality — quantized |
| `loss_gap_bp` | u16 | 0..10000 | Decision quality — quantized |
| `calibration_healthy` | bool | 0/1 | Flag |
| `safe_mode_active` | bool | 0/1 | Flag |
| `tick_utilization_bp` | u16 | 0..10000 | Budget — quantized |
| `controller_mode` | u8 | 0..2 | Budget — enum ordinal |
| `risk_tier` | u8 | 0..2 | Stratification — enum ordinal |

**Privacy guarantee:** FeatureVector is `Copy` + all-numeric. No `String`,
no `Value`, no `Vec<u8>`. Content leakage is structurally impossible.

## ExperienceOutcome Field Classification

| Field | Type | Cat | Rationale |
|-------|------|-----|-----------|
| `observed_ts_micros` | i64 | A | Timestamp |
| `label` | String | **D** | Freeform — validated by `contains_secret()` |
| `correct` | bool | A | Flag |
| `actual_loss` | Option\<f64\> | A | Numeric metric |
| `regret` | Option\<f64\> | A | Numeric metric |
| `evidence` | Option\<Value\> | **D** | Unschematized JSON — must be metric-level only |

## NonExecutionReason Field Classification

All variants are **Category B (Derived)** — structured enums with numeric fields.

| Variant | Fields | Cat | Rationale |
|---------|--------|-----|-----------|
| `BudgetExhausted` | budget_name (String), current (f64), threshold (f64) | B | Budget names are constrained vocabulary |
| `SafetyGate` | gate_name (String), risk_score (f64), gate_threshold (f64) | B | Gate names are constrained vocabulary |
| `DeliberateInaction` | no_action_loss (f64), best_action_loss (f64) | A | Pure numerics |
| `CalibrationFallback` | reason (String) | B | Should use constrained vocabulary |

## FeatureExtension Field Classification

| Field | Type | Cat | Rationale |
|-------|------|-----|-----------|
| `ext_version` | u16 | A | Schema version |
| `fields` | Vec\<(String, i64)\> | B | Keys should be constrained vocabulary; values are numeric |

---

## Secret Detection (`contains_secret()`)

Applied to Category D fields during `ExperienceRow::validate()`.

### Patterns Detected

| Pattern | Type | Match Method |
|---------|------|-------------|
| `AKIA...` / `ASIA...` | AWS access key | Prefix |
| `ghp_` / `gho_` / `ghu_` / `ghs_` / `ghr_` / `github_pat_` | GitHub token | Prefix |
| `xoxb-` / `xoxp-` / `xoxs-` / `xoxa-` | Slack token | Prefix |
| `sk-` / `sk-ant-` | OpenAI/Anthropic key | Prefix |
| `glpat-` | GitLab PAT | Prefix |
| `npm_` | npm token | Prefix |
| `pypi-AgEIcHlwaS5` | PyPI token | Prefix |
| `Bearer ` / `token ` | Authorization header | Prefix |
| `-----BEGIN ... -----` | PEM block | Regex |
| `eyJ...` (3-segment base64) | JWT | Regex |
| `AWS_SECRET_ACCESS_KEY` | AWS env var | Substring |
| `PRIVATE KEY` | Key material | Substring |
| `password=` / `secret=` / `api_key=` / `apikey=` | Credential assignment | Substring |

### Validation Integration

`ExperienceRow::validate()` checks Category D fields:
- `evidence_summary`: secret scan + max 256 chars
- `outcome.label`: secret scan
- `context` (serialized to string): secret scan

Callers should reject rows where `validate()` returns a non-empty list.

---

## Retention & Archive Rules

| Artifact | SQLite TTL | Git Archive | Enforced By |
|----------|-----------|-------------|-------------|
| Open experience rows | Max 30d | Never | Denylist |
| Resolved experience rows | 365d | Never | Denylist |
| Experience rollups | 730d | Never | Denylist |
| Evidence ledger entries | 30d | Never | BoundedDebugTail |
| Transparency cards | 365d | Never (folded) | Batched audit |
| Audit summaries | 730d | Archived | Aggregated only |
| Feature vectors | With parent | Never | Parent denylist |
