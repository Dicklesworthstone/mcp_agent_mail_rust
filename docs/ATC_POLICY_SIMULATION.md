# ATC Policy Simulation

`am atc simulate` is the read-only dry-run surface for testing a proposed
liveness policy bundle against historical ATC experience rows before changing
the live policy.

## Command

```bash
am atc simulate <scenario_id> \
  --policy <bundle.json|-> \
  --since <RFC3339|YYYY-MM-DD> \
  [--until <RFC3339|YYYY-MM-DD>] \
  [--subject <agent>] \
  [--dry-run-confidence <0.0..1.0>] \
  [--output <report.json>] \
  [--format table|json|toon]
```

`scenario_id` is an operator-supplied label for the historical slice you are
simulating. It is written into the report so repeated runs stay comparable.

## What It Does

1. Loads the proposed liveness policy bundle from disk or stdin.
2. Replays the historical `atc_experiences` window through the sealed DB replay
   read path.
3. Re-evaluates persisted liveness posteriors under the proposed policy.
   If the bundle contains a `candidate`, that artifact is simulated; otherwise
   the command simulates the `incumbent` artifact directly.
4. Reports which decisions would keep the same action, which would change, and
   which stratums move the most.
5. Optionally writes the structured JSON artifact to `--output`.

## Non-Invasive Contract

- The command is read-only against the primary mailbox DB.
- It uses the canonical replay read path instead of ATC write APIs.
- It never inserts rows into `atc_experiences` or `atc_experience_rollups`.

## Current Scope

- The current implementation re-evaluates **liveness** decisions only.
- Non-liveness decisions inside the selected historical window are reported as
  skipped, not silently mixed into the counterfactual.
- Counterfactual outcomes are not fabricated. The report carries observed
  outcome labels and actual loss when the original row recorded them.

## Example Policy Bundle

```json
{
  "schema_version": 1,
  "bundle_id": "atc-liveness-bundle-r12",
  "bundle_hash": "3d09f0d7f6e10f5c",
  "incumbent": {
    "schema_version": 1,
    "policy_id": "liveness-proposed-r12",
    "artifact_hash": "49cdd5c447fcd0ab",
    "suspicion_k": 3.0,
    "max_probes_per_tick": 3,
    "probe_recency_decay_secs": 60.0,
    "probe_gain_floor": 0.01,
    "probe_budget_fraction": 0.55,
    "conservative_probe_budget_fraction": 0.25,
    "release_guard_enabled": true,
    "losses": [
      [0.0, 2.0, 10.0],
      [2.0, 0.5, 3.0],
      [4.0, 1.2, 0.1]
    ]
  },
  "candidate": null
}
```

## Example Run

```bash
am atc simulate april-tightening \
  --policy ./docs/examples/atc-liveness-bundle-r12.json \
  --since 2026-04-01 \
  --until 2026-04-17 \
  --output tests/artifacts/atc/simulation_2026-04-17.json \
  --format table
```

Example human output:

```text
ATC simulation april-tightening
  policy bundle: atc-liveness-bundle-r12
  incumbent policy: liveness-proposed-r12
  period: 2026-04-01T00:00:00.000000Z .. 2026-04-17T23:59:59.999999Z

Summary
  experiences considered: 12430
  decisions evaluated: 891
  same action: 782
  different action: 67
  same action, different loss: 42
  avg expected-loss delta: -0.083
```

## How To Read The Report

- `same_action`: the proposed bundle agrees with the historical action.
- `different_action`: the proposed bundle would choose a different liveness
  action for the same posterior.
- `same_action, different loss`: action stays the same but the policy margin
  moves, which is useful for shadow validation.
- `by_stratum`: aggregate effect by `(subsystem, effect_kind, risk_tier)`.
- `notable_divergences`: top changed or highest-delta decisions for follow-up
  with `am atc explain <decision_id>`.

## Recommended Workflow

1. Export or author a proposed bundle JSON.
2. Simulate a recent historical window.
3. Review `different_action` and `by_stratum`.
4. Inspect the top deltas with `am atc explain <decision_id>`.
5. Only then decide whether the bundle is safe to promote.
