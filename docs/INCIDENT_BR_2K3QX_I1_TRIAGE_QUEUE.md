# br-2k3qx.9.1 - Mismatch Triage Queue + Root-Cause Assignment

Last updated: 2026-03-02 (RubyPrairie)

## Summary

All 5 symptom classes from the A4 classification matrix have been triaged.
All B-track, C-track, D-track, and F-track remediation beads are CLOSED.
G-track truth audits found 2 fixes (G3 LIKE wildcards, G4 body_md gaps) and
verified 2 surfaces clean (G5 threads, G6 projects/agents).
H-track oracle tests (H2, H3, H4) all CLOSED with oracle harness validated.

## Triage Queue

### TIER 1: Critical/High (All REMEDIATED)

| ID | Symptom | Root Cause | Module | Fix Applied | Test Backfill | Status |
|----|---------|-----------|--------|-------------|---------------|--------|
| M1 | S2: Messages screen empty body for live events | `body_md: String::new()` in live-event builder | `messages.rs:2288` | B2 (br-2k3qx.2.2) | E2 body rendering tests | CLOSED |
| M2 | S1: Dashboard recent msg no GFM | `RecentMessagePreview` uses event metadata not full body | `dashboard.rs:346` | B1 (br-2k3qx.2.1) | E1 seeded truth tests | CLOSED |
| M3 | S5: Health URL unauthorized | Auth gate before route dispatch | `lib.rs:4136` | D1-D4 (br-2k3qx.4.1-4.4) | E4 health workflow tests | CLOSED |
| M4 | S3: Threads false-empty (LIKE wildcard) | Unescaped `%`/`_` in filter predicates | `threads.rs:1842-1863` | G3 fix | G3 param-sweep tests (3 tests) | CLOSED |
| M5 | S4: Agents/Projects cardinality drift | DB COUNT vs list mismatch in diagnostics | `agents.rs:323`, `projects.rs:157` | B4-B5 (br-2k3qx.2.4-2.5) | E7 non-empty smoke matrix | CLOSED |

### TIER 2: Medium (All REMEDIATED)

| ID | Symptom | Root Cause | Module | Fix Applied | Test Backfill | Status |
|----|---------|-----------|--------|-------------|---------------|--------|
| M6 | Search results missing body_md | `body_md` not in SELECT for search synthetic events | `search.rs:1665` | G4 fix | G4 body propagation tests (5 tests) | CLOSED |
| M7 | Explorer inbound/outbound missing body_md | Explorer queries omitted body_md column | `explorer.rs:687,728,753,778` | G4 fix | G4 body propagation tests | CLOSED |
| M8 | Agents sort inconsistency | Case-sensitive sort causing instability | `agents.rs` | G3 fix (COLLATE NOCASE) | G3 param-sweep tests | CLOSED |
| M9 | Projects sort inconsistency | Case-sensitive sort causing instability | `projects.rs` | G3 fix (COLLATE NOCASE) | G3 param-sweep tests | CLOSED |
| M10 | Thread orphan handling | Orphan exclusion needed verification | `threads.rs:772,818` | G5 verified clean | G5 thread semantics tests (4 tests) | CLOSED |

### TIER 3: Low/Edge (All VERIFIED)

| ID | Symptom | Root Cause | Module | Fix Applied | Test Backfill | Status |
|----|---------|-----------|--------|-------------|---------------|--------|
| M11 | Agent uniqueness across projects | Global agent list cardinality check | `agents_list` | G6 verified clean | G6 scope audit tests (4 tests) | CLOSED |
| M12 | Project scoping isolation | Entity counts per project vs global | `projects_list` | G6 verified clean | G6 scope audit tests | CLOSED |
| M13 | Thread sort stability | Deterministic sort under tied timestamps | `threads.rs` | G5 verified clean | G5 sort stability tests | CLOSED |

### ANCILLARY FINDINGS (from A3 harness)

| ID | Finding | Module | Severity | Status |
|----|---------|--------|----------|--------|
| A1 | Attachments robot query `ORDER BY m.created_ts DESC` without m in SELECT | `robot.rs:3576-3581` | Low | Not in scope (CLI robot, not TUI/API surface) |
| A2 | DB schema integrity probe failure on high-cardinality fixture | Schema repair | Low | Seed-specific; production DBs unaffected |
| A3 | Mail root capture timeout under auth | Route/auth ordering | High | Fixed by D-track (br-2k3qx.4.x) |

## Cross-Reference: Remediation Beads

All remediation beads referenced in the classification matrix are CLOSED:

- **B-track** (Data Truthfulness Repairs): B1-B8 all CLOSED
- **C-track** (Diagnostic Instrumentation): C1, C3, C5 all CLOSED
- **D-track** (Health/Auth Workflow): D1-D5 all CLOSED
- **F-track** (Security Hardening): F1-F4 all CLOSED
- **G-track** (Truth Audits): G1-G6 all CLOSED (G3 + G4 had fixes; G5 + G6 verified clean)
- **H-track** (Oracle Tests): H2-H4 all CLOSED

## Test Coverage Summary

| Track | Tests Added | Coverage |
|-------|------------|----------|
| E1: Seeded Tab Truth | 13 integration tests | All core TUI surfaces |
| E2: Body Rendering | Fixture matrix tests | Dashboard + messages body_md |
| E3: Thread Integrity | 5 integration tests | Thread list + detail + counts |
| E4: Health Workflow | Fixture matrix tests | Auth success + unauthorized |
| E7: Non-Empty Smoke | 9 integration tests | All core surfaces non-empty |
| E8: Unit Invariants | 17 unit tests | query_params, diagnostics, config, truth assertions |
| E9: E2E Matrix | 50 E2E assertions | Full workflow: seed + capture + truth comparison |
| G3: Param Sweep | 3 tests | LIKE wildcards, case-insensitive sort |
| G4: Body Propagation | 5 tests | Search + explorer body_md |
| G5: Thread Semantics | 4 tests | Orphan exclusion, sort stability |
| G6: Scope Audit | 4 tests | Agent uniqueness, project scoping |

## Conclusion

**All 13 triaged mismatches are REMEDIATED or VERIFIED CLEAN.**

No open mismatches remain that require I2 (Critical/High), I3 (Medium), or I4 (Low/Edge)
remediation waves. The I-track can proceed directly to I5 (Incident Closure Verification)
once the remaining E5/E6/H5 CI gate beads are closed.
