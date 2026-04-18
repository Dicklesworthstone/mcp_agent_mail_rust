# Reality Check Diff — 2026-04-18 vs 2026-04-17 Epic Set

## Comparison Basis

No standalone committed `reality-check_2026-04-17_findings.md` artifact exists in the repo.
This diff therefore compares the epilogue against the five epic parents created from that
reality-check session.

## Closure Matrix

| Original gap bucket | Tracking epic | 2026-04-18 state | Evidence |
|---|---|---|---|
| Documentation counts/naming drift | `br-o217s` | Closed | `README.md`, `AGENTS.md`, `docs/DOC_SWEEP_AUDIT_2026-04-18.md`, `CHANGELOG.md` |
| WASM/browser oversell | `br-il53l` | Closed (retire branch executed) | deferred spec, changelog, README browser copy |
| ATC learning loop not wired end-to-end | `br-bn0vb` | Closed | ATC hot-path docs, robot/TUI/README surfaces, epic child closure set |
| Archive write-path perf massively over budget | `br-8qdh0` | Closed | perf docs, rollback/runbook docs, changelog note, closed epic parent |
| Conformance fixture/resource coverage gaps | `br-a2k3h` | Closed | conformance audit + resource audit + closed epic parent |

## Residual Active Beads

| Bead | Interpretation |
|---|---|
| `br-bn0vb.31` | ATC enhancement (`am atc simulate`), not a blocker to the original ATC closure claim |
| `br-8qdh0.13` | Read-path perf characterization, complementary to the already-closed write-path perf epic |

## Epilogue-Only Drift Found

| Item | Classification | Resolution |
|---|---|---|
| `/mail/ws-state` described as deferred in the browser-parity deferred spec even though robot/TUI still use it as a supported polling endpoint | NEW minor drift | Fixed inline in `README.md` and `docs/SPEC-browser-parity-contract-deferred.md` |

## Net Result

- Original epic-scale gap categories still open: **0**
- Newly discovered epic-scale gap categories: **0**
- Minor doc drift items found during epilogue: **1**
- Minor doc drift items fixed inside epilogue bead: **1**

This repo is no longer missing a category of work that requires a new post-epic umbrella plan.
