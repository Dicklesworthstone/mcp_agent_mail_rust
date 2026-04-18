# Release Readiness Template

Copy this file into a release-specific note before promoting a train.

## Release Identity

| Field | Value |
|------|-------|
| Release name | |
| Version | |
| Train | |
| Release lead | |
| UTC decision window | |
| Previous release | |

## Scope

### Included beads

| Bead | Outcome | Status | Notes |
|------|---------|--------|-------|
| | | | |

### Explicitly excluded beads

| Bead | Reason held back | Follow-up train |
|------|------------------|-----------------|
| | | |

## Dependency Check

| Dependency | Required? | Status | Evidence |
|-----------|-----------|--------|----------|
| Upstream blocking beads closed | Yes | | |
| Docs updated for shipped behavior | Yes | | |
| Installer checksum refresh needed? | If version changed | | |
| External fixture regeneration complete | If applicable | | |
| Rollback owner assigned | Yes | | |

## Shippable Criteria

Document the train-specific definition of shippable from
[`RELEASE_TRAIN_PLAN.md`](RELEASE_TRAIN_PLAN.md).

### Summary

| Criterion | Status | Evidence |
|----------|--------|----------|
| | | |

## Verification Evidence

### Required commands

| Command | Result | Artifact / note |
|--------|--------|------------------|
| `rch exec -- cargo check ...` | | |
| `rch exec -- cargo test ...` | | |
| `rch exec -- cargo clippy ...` | | |
| Targeted `am e2e run ...` | | |
| `am doctor check` | | |

### Additional train-specific evidence

| Evidence type | Path / link | Reviewed by |
|--------------|-------------|-------------|
| Perf artifacts | | |
| Canary logs | | |
| Operator screenshots / notes | | |
| Recovery / rollback rehearsal | | |

## Canary Plan

| Item | Value |
|------|-------|
| Canary cohort | |
| Start time (UTC) | |
| Minimum dwell | |
| Success signals | |
| Abort triggers | |
| Monitoring owner | |

## Rollback Plan

### Primary rollback

- Trigger:
- Command or config change:
- Expected blast radius:
- Verification after rollback:

### Secondary rollback

- Trigger:
- Command or redeploy action:
- Verification after rollback:

## External Coordination

| Dependency | Owner | Status | Notes |
|-----------|-------|--------|-------|
| Python reference / fixture regen | | | |
| Installer checksum refresh | | | |
| Release notes / changelog | | | |
| Docs / README / runbook updates | | | |

## Sign-Off Ledger

| Role | Name | Decision | UTC timestamp | Notes |
|-----|------|----------|---------------|-------|
| Runtime owner | | | | |
| DB / storage owner | | | | |
| Operator docs owner | | | | |
| Repo owner | | | | |

## Final Promotion Decision

| Field | Value |
|------|-------|
| Decision (`go` / `no-go`) | |
| Promotion command / action | |
| Release artifact link | |
| Follow-up beads created? | |

## Post-Release Follow-Through

| Item | Owner | Due | Status |
|------|-------|-----|--------|
| Canary retrospective | | | |
| Rollback rehearsal refresh | | | |
| Changelog finalized | | | |
| Deferred-bead re-triage | | | |
