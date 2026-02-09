# br-33ha: Comprehensive Test Coverage Hardening (Unit + E2E)

## Current State (post-revision, 2026-02-09)
- **2,263+ workspace tests**, 0 failures, 7 ignored
- **0 clippy warnings**
- **No mocks/fakes** except: toon stub encoder scripts + LLM stub mode (both acceptable)
- Core/DB/Storage/Tools/Guard/Share: ~95% function coverage
- **Server/TUI: DONE — ~900+ unit tests added** (Tracks 1-2 fully closed)
- **CLI: DONE — 296+ inline tests, 90+ external tests** (Track 2 fully closed)
- **28+ E2E suites**, 135+ test cases, excellent e2e_lib.sh harness

## Completed Tracks (no remaining work)

### Track 1 (br-33ha.2): Server/TUI Crate Unit Tests — CLOSED
All 10 tasks closed. ~900+ tests added across 16 modules.

### Track 2 (br-33ha.3): Core/CLI Isolated Unit Tests — CLOSED
All 4 tasks closed. config.rs, diagnostics, output.rs, context.rs all covered.

## Remaining Tracks (9 task beads, 4 epic beads)

### Track 3 (br-33ha.4): E2E HTTP-Level Tests [P2, 2 remaining of 4]
- ~~br-33ha.4.1: Product bus tools E2E~~ ✓ CLOSED
- ~~br-33ha.4.2: Resource error cases E2E~~ ✓ CLOSED (test_resource_errors.sh)
- **br-33ha.4.3**: Health endpoints via HTTP — 5 cases, 10+ assertions
  - Liveness (/healthz, /health/liveness), readiness (/health, /health/readiness)
  - Broken DB (503), auth bypass, path prefix matching
- **br-33ha.4.4**: CORS preflight behavior — 6 cases, 12+ assertions
  - Disabled default, wildcard, specific origins, credentials, OPTIONS handling

### Track 4 (br-33ha.5): E2E Edge Cases [P2, 1 remaining of 5]
- ~~br-33ha.5.1: Unicode/emoji~~ ✓ CLOSED (test_unicode.sh)
- ~~br-33ha.5.2: Large inputs~~ ✓ CLOSED (test_large_inputs.sh)
- ~~br-33ha.5.3: Null/missing fields~~ ✓ CLOSED (test_null_fields.sh)
- ~~br-33ha.5.4: Malformed protocol~~ ✓ CLOSED (test_malformed_rpc.sh)
- **br-33ha.5.5**: DB corruption recovery — 6 cases, 10+ assertions
  - Missing DB, truncated file, zero-byte, PRAGMA integrity, WAL corruption, read-only dir

### Track 5 (br-33ha.6): E2E Multi-Agent Production Scenarios [P2, 3 remaining of 4]
- ~~br-33ha.6.3: Crash + state recovery~~ ✓ CLOSED (test_crash_restart.sh)
- **br-33ha.6.1**: Cross-project messaging — 6 cases, 15+ assertions
  - Two projects, contact request/approve, cross-project send, policy blocks, thread replies
- **br-33ha.6.2**: Concurrent agent scenarios — 6 cases, 12+ assertions
  - Reservation conflicts, release unblocks, concurrent sends, racing, TTL expiry
- **br-33ha.6.4**: Contact policy enforcement — 10 cases, 15+ assertions ← **NEW**
  - open/auto/contacts_only/block_all, bypass rules (thread, shared reservations), auto_contact_if_blocked

### Track 6 (br-33ha.7): E2E Quality & Observability [P3, 3 tasks]
- **br-33ha.7.1**: Audit ~28 E2E suites for consistent e2e_lib.sh usage
  - Check sourcing, banners, asserts, summary, artifact capture
- **br-33ha.7.2**: JSON summary output for CI aggregation [blocked by .7.1]
  - e2e_json_summary, e2e_case_start/end, aggregate runner report
- **br-33ha.7.3**: Tool metrics + query tracking E2E — 6 cases, 10+ assertions
  - Initial state, increment after calls, error counting, cluster metadata, filtering

## Dependency Graph
```
br-33ha.7.1 (logging audit)
  └── blocks → br-33ha.7.2 (JSON summary output)
```

All other tasks are ready (no blockers).

## Expected Remaining Work
- ~85+ new E2E assertions across 6 new test scripts (Tracks 3-5)
- Logging/quality improvements across ~28 existing suites (Track 6)
- JSON CI report infrastructure (Track 6)
