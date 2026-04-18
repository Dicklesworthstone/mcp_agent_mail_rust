# Release Train Plan

Staged sequencing for the current reality-check and idea-wizard outcomes.

This document answers a different question than the existing rollout docs:

- `docs/ROLLOUT_PLAYBOOK.md` explains how to promote one risky surface safely.
- `docs/RELEASE_CHECKLIST.md` captures gate criteria for a release candidate.
- This plan decides which beads belong together in the next few releases so we
  do not ship unrelated risk at the same time.

## Planning Rules

1. Keep documentation-truth changes separate from risky runtime changes.
2. Keep archive durability/performance work separate from ATC live-mode changes.
3. Promote ATC in two steps: shadow/validation first, broader live rollout later.
4. Treat additive schema changes as safer than behavior changes, but do not mix
   them with unrelated storage-path risk unless they share the same rollback.
5. Do not assign calendar dates here. Sequence is relative to readiness.

## Train Summary

| Train | Suggested version band | Scope | Risk | Canary window | Rollback |
|------|-------------------------|-------|------|---------------|----------|
| Train A | `v0.2.x` patch train | Docs truth, deferred-browser cleanup, conformance/docs alignment, messaging clarity | Low | 24h docs/operator smoke | Revert docs/copy commits |
| Train B | `v0.3.0` | Archive durability + batch-write perf + rollback controls + quality-gate ergonomics | Medium | 72h single-project canary | Switch archive path to last-known-good mode, redeploy previous binary |
| Train C | `v0.3.1` | Operator UX polish, fixture automation/backfill, non-risky CLI/TUI improvements | Low-medium | 24-48h operator canary | Revert UX/docs/features without data migration |
| Train D | `v0.4.0` | ATC seam closure in `shadow` or opt-in validation mode, observability, replay/retention, E2E coverage | High | 7-day single-project ATC canary | Set `AM_ATC_WRITE_MODE=off` or `shadow`, keep additive schema |
| Train E | `v0.5.0` | ATC broader rollout, docs disclaimer removal, release cleanup, reality-check epilogue | High | 25% -> 50% -> 100% ringed rollout | Demote ATC back to `shadow` or `off`, leave schema in place |

## Train A: Documentation Truth and Safe Cleanup

### Target outcomes

- Documentation alignment and count corrections
- Browser/WASM retirement or deferment copy
- Threat model, durability messaging, and operator-facing clarity
- Conformance and fixture/docs audits that do not change live runtime behavior

### Candidate beads

- `br-o217s*` documentation-alignment outcomes
- `br-il53l*` deferred-browser / experimental-WASM cleanup outcomes
- `br-rqv3i` startup/doctor root-cause clarity
- `br-a2k3h*` conformance audit and fixture documentation work
- `br-97gc6.5.2` mailbox durability audit outputs

### Definition of shippable

- README, AGENTS, runbooks, and changelog agree on the current shipped surface
- Deferred features are labeled consistently as deferred or experimental
- Startup and doctor messaging point to concrete root causes and recovery hints
- Conformance docs and fixture inventories match the current tool/resource count

### Canary plan

- One operator workstation
- One fresh install smoke test
- One upgrade-from-prior-release smoke test
- Verify docs-linked commands still exist and produce the described output

### Rollback

- Revert the docs/messaging commits only
- No database or archive repair required

### External dependencies

- None beyond normal docs review
- If a version bump happens in the same patch release, also refresh installer
  checksums in `/dp/agentic_coding_flywheel_setup`

## Train B: Archive Durability and Performance

### Target outcomes

- Archive batch-write performance fixes
- Rollback controls for the performance path
- Cross-filesystem verification and startup/p99.9 guard work
- Unified local quality gate improvements that reduce release drift

### Candidate beads

- `br-8qdh0*` archive performance track
- `br-bb0gt.1` unified `am check` gate if it lands cleanly with this train
- Any follow-up durability/runbook work that shares the same rollback story

### Definition of shippable

- Warm-path archive batch writes stay within budget in checked-in artifacts
- Rollback mechanism exists and is documented in operator-facing docs
- `am doctor check` and archive validation show no new drift after canary runs
- Release verification demonstrates both the new path and the rollback path

### Canary plan

- Single canary project for 72 hours
- Run archive-heavy workflows with attachments and concurrent readers
- Capture:
  - archive write benchmark artifacts
  - `am doctor check` output before and after canary
  - one restart/recovery rehearsal

### Rollback

- Prefer runtime/config rollback first if the perf path is feature-gated
- Otherwise redeploy last-known-good release and rerun archive health checks
- Do not mix rollback with ATC or unrelated schema changes

### External dependencies

- Performance artifacts committed under the current benchmark conventions
- If release version changes, refresh installer checksums in ACFS repo
- Coordinate with any benchmark-host or CI capacity constraints before promotion

## Train C: Operator UX and Fixture Hygiene

### Target outcomes

- TUI/CLI usability improvements
- Help, health, registry, and integration-surface polish
- Fixture regeneration automation and conformance backfill
- Lower-risk operator ergonomics that do not alter persistence semantics

### Candidate beads

- `br-bb0gt*` UX enhancements not coupled to Train B
- `br-a2k3h.7` fixture regeneration pipeline
- Remaining doc-backed CLI/TUI improvements with isolated rollback

### Definition of shippable

- Operator help and health surfaces are internally consistent
- Fixture automation is documented and reproducible
- No new UI or CLI command drifts from README/runbook/docs tables
- Targeted crate tests and the relevant E2E suites stay green

### Canary plan

- 24-48h operator-only canary
- At least one keyboard-only or robot-only workflow pass
- One fixture regeneration rehearsal with artifacts recorded

### Rollback

- Revert the UX/fixture commits
- No data-path rollback or migration work needed

### External dependencies

- Python reference fixture regeneration where conformance parity needs it
- Any related doc tables in README/AGENTS should be updated in the same train

## Train D: ATC Shadow Validation

### Target outcomes

- Close remaining ATC seam gaps in persistence/query/replay/retention
- Add end-to-end validation and observability
- Prove learning-loop correctness without turning ATC into the default live path

### Candidate beads

- `br-bn0vb*` seam work needed for:
  - append/resolve wiring
  - open-experience queries
  - retention/compaction/replay
  - observability and simulation
  - E2E validation
- Documentation that still describes ATC as architecture-phase should remain
  until this train has passed canary

### Definition of shippable

- ATC schema and runtime seams are additive and restart-safe
- `AM_ATC_WRITE_MODE=shadow` or equivalent opt-in mode is the recommended
  rollout default
- E2E coverage proves dispatch -> experience write -> outcome resolution ->
  rollup refresh on real server paths
- Replay, retention, and recovery behavior are documented and exercised

### Canary plan

- Single project or one small agent pool
- Minimum 7-day simulated or real canary window
- Capture:
  - ATC summary snapshots
  - outcome-resolution and overdue-sweep evidence
  - retention/compaction results
  - operator comprehension feedback from TUI/robot surfaces

### Rollback

- Set `AM_ATC_WRITE_MODE=off` or `shadow`
- Keep additive schema in place; do not attempt emergency down-migration
- If needed, stop the ATC operator runtime while preserving mailbox operation

### External dependencies

- Backup/reconstruct compatibility for ATC tables
- Soak/perf infrastructure for long-running validation
- Coordination with owners of operator surfaces that render ATC state

## Train E: ATC Broader Promotion and Cleanup

### Target outcomes

- Promote ATC from shadow validation to broader rollout
- Remove the README architecture-phase disclaimer only after Train D evidence
- Close release cleanup and meta/documentation follow-through

### Candidate beads

- `br-bn0vb.17` disclaimer removal
- Remaining ATC rollout and cleanup beads
- `br-ldpdv` reality-check epilogue once the release sequence is complete

### Definition of shippable

- Train D canary completed with no unresolved correctness or operator-clarity gaps
- Release notes explain what ATC now does in production and what remains guarded
- Rollback to `shadow` or `off` is rehearsed once before broader promotion
- Cleanup docs no longer describe ATC as hypothetical where it is now real

### Canary plan

- 25% -> 50% -> 100% ringed rollout
- Hold at each ring long enough to inspect:
  - ATC outcome distributions
  - operator alerts/noise rate
  - storage and perf regressions

### Rollback

- Demote ATC from `live` back to `shadow` or `off`
- Revert operator-facing defaults before touching any stored data
- File follow-up beads instead of trying to scrub historical ATC rows

### External dependencies

- Changelog/release-note preparation
- Repo-owner sign-off on ATC behavior wording
- Any public-facing docs or demo materials that mention ATC

## Cross-Train Constraints

### Hard sequencing

1. Train A can ship independently.
2. Train B should ship before Train C only if operator gate work is needed for
   its canary; otherwise B and C may swap.
3. Train D must not ship before Train B if archive durability questions remain.
4. Train E must not ship before Train D completes its canary and soak evidence.

### Things that should not be mixed

- Archive perf risk and ATC live promotion in the same release
- Large docs-truth sweeps and emergency rollback/mechanism changes
- ATC disclaimer removal before live-path validation exists

## Release Readiness Workflow

Before promoting any train:

1. Copy `docs/RELEASE_READINESS_TEMPLATE.md` to a release-specific note.
2. Fill in included beads, evidence, canary owner, and rollback owner.
3. Link the filled template from the active bead thread and release notes draft.
4. Verify the release still matches the train boundary defined here.

## Relationship to Existing Docs

- Use `docs/ROLLOUT_PLAYBOOK.md` for the mechanics of promotion and rollback.
- Use `docs/RELEASE_CHECKLIST.md` for gate evidence and sign-off.
- Use this document to decide which outcomes belong in the next release at all.
