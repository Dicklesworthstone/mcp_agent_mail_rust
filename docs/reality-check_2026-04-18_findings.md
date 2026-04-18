# Reality Check Epilogue — 2026-04-18

## Scope

Post-implementation epilogue for the five epic set created from the 2026-04-17 reality-check:

- `br-o217s` — documentation alignment
- `br-il53l` — WASM/browser ship-or-retire decision
- `br-bn0vb` — ATC learning loop closure
- `br-8qdh0` — archive batch-write performance
- `br-a2k3h` — conformance fixture/resource coverage

This run cross-checked the live repo surface in:

- `README.md`
- `AGENTS.md`
- `CHANGELOG.md`
- `docs/ATC_HOT_PATH_WIRING.md`
- `docs/CONFORMANCE_AUDIT_2026-04-18.md`
- `docs/RESOURCE_COVERAGE_AUDIT.md`
- `docs/DOC_SWEEP_AUDIT_2026-04-18.md`
- `docs/SPEC-browser-parity-contract-deferred.md`
- `.beads/beads.db`
- spot checks in `crates/mcp-agent-mail-cli/src/robot.rs` and `crates/mcp-agent-mail-server/src/lib.rs`

## Important Note On Comparison Source

The original 2026-04-17 findings were operationalized into the five epic parents above, but a
standalone committed `reality-check_2026-04-17_findings.md` artifact does not exist in this tree.
For this epilogue, the five epic parents and their shipped child beads are treated as the
traceable proxy for the original findings set.

## Executive Summary

The repo now materially delivers on the current README/AGENTS vision.

What is now true:

- The shipped surface is documented honestly as **37 MCP tools**, **25 resources**, and a
  **16-screen** TUI.
- The browser TUI mirror/WASM branch is explicitly deferred rather than oversold.
- The ATC learning loop is live enough that the architecture-phase disclaimer could be retired.
- Archive write-path performance is back under budget and guarded by CI/perf documentation.
- Conformance coverage now documents the full 37-tool / 25-resource surface, including the three
  Rust-native identity tools.

Residual open work still exists, but it no longer represents the original five-gap reality-check
set failing to deliver. The remaining active beads are post-epic enhancements:

- `br-bn0vb.31` — `am atc simulate`
- `br-8qdh0.13` — read-path perf characterization

## Findings

| Category | Finding | Status | Action |
|---|---|---|---|
| DONE | Documentation count/naming drift that triggered `br-o217s` is closed | Closed | Verified against README, AGENTS, doc audit, and changelog |
| DONE | Browser/WASM oversell that triggered `br-il53l` is closed | Closed | README and changelog now present it as deferred/retired work rather than a shipped promise |
| DONE | ATC loop being architecture-only is no longer true | Closed | Hot-path writes, outcome resolution, robot/TUI snapshot consumers, perf guard, soak, and README disclaimer removal all landed |
| DONE | Archive batch-write performance gap is closed | Closed | Epic parent is closed and README/changelog/doc artifacts reflect the under-budget path |
| DONE | Conformance fixture/resource coverage gap is closed | Closed | Audit artifacts and AGENTS/README language align with the current parity model |
| NEW (minor) | Deferred-browser spec overstated `/mail/ws-state` as deferred even though robot/TUI still use it as a supported polling endpoint | Fixed inline | Updated README and `docs/SPEC-browser-parity-contract-deferred.md` during this bead |

## Gap Assessment

### 1. What specifically is working right now?

- Core MCP coordination surface: tools, resources, robot mode, mail archive, DB indexing, and TUI.
- Server-rendered `/mail/*` web UI for human oversight.
- ATC learning loop with live hot-path writes and operator-facing summaries.
- Conformance/documentation/perf drift guards added during the epic run.
- `am check` and cross-epic integration coverage landed as release-quality gates.

### 2. What is not working or not yet implemented?

- The deferred browser mirror remains deferred by design:
  - `/web-dashboard/*` is not a supported live surface.
  - `/mail/ws-input` remains intentionally unimplemented for browser control ingress.
- Two tracked enhancements are still active:
  - `br-bn0vb.31` simulation/dry-run tooling
  - `br-8qdh0.13` read-path perf characterization

### 3. What is blocking us from the README vision?

Nothing found here rises to a new epic-scale blocker against the current README/AGENTS vision.
The remaining gaps are either:

- explicitly deferred product choices, or
- incremental enhancements already tracked by existing beads.

### 4. If we implemented all open and in-progress beads, would we close the gap completely?

Yes for the current documented vision.

The remaining active beads would tighten the product further, but the repo is no longer in the
state that triggered the 2026-04-17 reality-check. Those beads extend or harden already-landed
capabilities rather than closing a missing foundational promise.

### 5. What goals from the vision are not covered by any existing bead?

No new untracked gap category was identified during this epilogue.

The only post-epic drift found was the `/mail/ws-state` documentation mismatch, and that was
closed directly inside this bead rather than needing a new follow-up bead.

## Verification Notes

Evidence gathered during this epilogue included:

- all five original epic parents are `closed` in `.beads/beads.db`
- `README.md` and `AGENTS.md` both reflect the 37-tool / 25-resource / 16-screen surface
- `crates/mcp-agent-mail-cli/src/robot.rs` still polls `/mail/ws-state` for `am robot atc`
- `crates/mcp-agent-mail-server/src/lib.rs` serves `GET /mail/ws-state` and returns `501` for
  `/mail/ws-input` and `/web-dashboard/*`

## Outcome

- `REGRESSED`: 0
- `PERSISTENT`: 0 new blockers
- `NEW`: 1 minor doc drift item, fixed inline in this bead
- New follow-up beads created: 0

The project no longer shows a new-category gap relative to the 2026-04-17 reality-check set.
