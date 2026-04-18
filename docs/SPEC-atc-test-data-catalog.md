# SPEC: ATC Test-Data Catalog

Audited 2026-04-18 for `br-bn0vb.25`.

This catalog defines the canonical ATC labeling fixtures that future unit,
integration, replay, and conformance tests should share. The goal is not to
materialize the full cartesian product of every ATC dimension. The goal is to
pin one stable scenario for every outcome family, every censor reason, every
budget-health state, and the contention patterns that change labeling behavior.

## Scope

The catalog covers these contract surfaces:

- `OutcomeLabel` in [crates/mcp-agent-mail-core/src/atc_labeling.rs](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-core/src/atc_labeling.rs)
- `CensorReason` in [crates/mcp-agent-mail-core/src/atc_labeling.rs](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-core/src/atc_labeling.rs)
- `BudgetHealth` in [crates/mcp-agent-mail-core/src/atc_risk_budgets.rs](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-core/src/atc_risk_budgets.rs)
- contamination and contention semantics in [crates/mcp-agent-mail-core/src/atc_contamination.rs](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-core/src/atc_contamination.rs)

The source-of-truth fixture payload lives at [tests/fixtures/atc_label_catalog.json](/data/projects/mcp_agent_mail_rust/tests/fixtures/atc_label_catalog.json).

## Coverage Rules

- Every `OutcomeLabel` family must appear at least once: `success`, `failure`, `correct`, `incorrect`, `censored`, `non_execution`.
- Every `CensorReason` variant must appear at least once. If a variant is present in the enum but unreachable in v1 logic, the catalog keeps a reserved placeholder scenario instead of inventing impossible runtime input.
- Every `BudgetHealth` state must appear at least once: `healthy`, `stressed`, `cooling_down`, `blocking`.
- Multi-agent contention must cover both tolerated overlap and forced censoring.
- Attribution timing must cover: immediate-valid, valid-after-min-delay, no-evidence-yet, expired, and rule-preempted cases.

## Reserved Reachability

`CensorReason::ProjectClosed` exists in the sealed enum but is not emitted by the
current v1 labeling automaton. Runtime labeling currently collapses project loss
into `SubjectDeparted`. The catalog therefore includes one `reserved_unreachable_v1`
scenario for `project_closed` so future work can detect when the variant becomes
reachable and must be backed by a real fixture lane.

## Canonical Scenario Matrix

| Scenario ID | Primary coverage | Budget | Contention | Window case | Reachability |
|---|---|---|---|---|---|
| `probe_responded_healthy_isolated` | `correct` | `healthy` | `isolated` | immediate valid evidence | reachable |
| `advisory_improved_stressed_isolated` | `success` | `stressed` | `isolated` | after min delay | reachable |
| `backpressure_worsened_stressed_isolated` | `failure` | `stressed` | `isolated` | after min delay | reachable |
| `release_false_positive_cooling_down_isolated` | `incorrect` | `cooling_down` | `isolated` | immediate valid evidence | reachable |
| `force_reservation_resolved_healthy_isolated` | `correct` | `healthy` | `isolated` | immediate valid evidence | reachable |
| `no_action_stable_healthy_isolated` | `non_execution` correct | `healthy` | `isolated` | after min delay | reachable |
| `budget_exhausted_worsened_blocking` | `non_execution` incorrect | `blocking` | `isolated` | after min delay | reachable |
| `routing_overlap_tolerated_stressed` | `success` with allowed overlap | `stressed` | `one_peer_overlap_tolerated` | after min delay | reachable |
| `probe_window_expired_no_evidence` | `censored/window_expired` | `healthy` | `isolated` | no evidence after max window | reachable |
| `no_action_subject_departed` | `censored/subject_departed` | `healthy` | `isolated` | subject leaves before observation | reachable |
| `release_operator_override_blocking` | `censored/concurrent_operator_change` | `blocking` | `manual_operator_override` | override before observation | reachable |
| `probe_overlap_censored_multi_agent` | `censored/overlapping_interventions` | `healthy` | `high_force_overlap_censored` | overlap exceeds tolerance | reachable |
| `advisory_exogenous_recovery_stressed` | `censored/exogenous_recovery` | `stressed` | `isolated` | after min delay | reachable |
| `non_execution_missing_reason_ambiguous` | `censored/ambiguous_result` | `healthy` | `isolated` | invalid non-execution metadata | reachable |
| `backpressure_awaiting_evidence_insufficient` | `censored/insufficient_evidence` | `healthy` | `isolated` | still within window, no evidence yet | reachable |
| `project_closed_reserved_placeholder` | `censored/project_closed` | `blocking` | `isolated` | reserved placeholder | reserved_unreachable_v1 |
| `calibration_fallback_stable_cooling_down` | `non_execution` correct | `cooling_down` | `isolated` | after min delay | reachable |

## Fixture Contract

Each fixture row in the JSON catalog must declare:

- stable `id`
- short `description`
- `effect_kind` and lifecycle `state`
- scenario metadata for `budget_health`, `contention_profile`, and `attribution_window_case`
- the expected `label_family`
- the expected `rule_id`
- optional `censor_reason`
- `reachability`

Downstream tests should consume this file as a matrix, not as prose. The doc is
the audit narrative; the JSON file is the reusable machine contract.
