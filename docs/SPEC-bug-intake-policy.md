# Bug Intake, Closure, and Reopen Policy

Canonical policy for the repo-wide bug-burn audit program (br-97gc6).

**Bead**: br-97gc6.1.3
**Parent**: br-97gc6.1 (Control plane: scope, audit rubric, and intake workflow)
**Last Updated**: 2026-04-02

---

## 1. When to File a Bug

A bug bead MUST be filed when any of the following conditions are met:

### 1.1 Mandatory Filing Triggers

| Trigger | Example | Minimum Severity |
|---------|---------|-----------------|
| Incorrect output for valid input | `send_message` silently drops cc recipients | P0 |
| Data corruption or silent data loss | Timestamp truncation, body_md empty on write | P0 |
| Panic, unwrap failure, or unhandled error on a reachable code path | `unwrap()` on user-supplied input | P0 |
| Security boundary violation | Auth bypass, agent reads another agent's mail without permission | P0 |
| Regression from a previously passing test or verified behavior | Conformance test that was green now fails | P0 |
| Unintentional deviation from a supported compatibility contract | Rust `list_contacts` drops a field used by existing clients without a migration note | P1 |
| Integer overflow, underflow, or unsaturated arithmetic on user-facing values | `created_ts - updated_ts` without `saturating_sub` | P1 |
| Resource leak (DB connections, file handles, WAL growth) | Pool exhaustion under normal load | P1 |
| Incorrect error message or misleading diagnostic | Error says "not found" when the real cause is a permission check | P2 |
| Documentation claims behavior that code does not implement | SPEC doc says "retries 3 times" but code does not retry | P2 |

### 1.2 Do NOT File When

- The behavior is a **known limitation** already tracked in a bead or documented in a SPEC.
- The behavior is an **intentional Rust-side improvement** over the legacy Python
  implementation, is documented as such, and has native invariant/real-path
  coverage. Python is a compatibility fixture source, not the product authority.
- The observation is a **style preference** (formatting, naming convention) with no correctness impact -- use a separate cleanup bead instead.
- The issue is in an **external dependency** outside this workspace (sqlmodel_rust, frankensqlite, asupersync, fastmcp_rust) -- file in the blocker ledger (br-97gc6.3.3) instead.
- The finding is a **false alarm** from a static analysis tool that has been verified as safe -- record as a false-alarm note on the audit lane bead, not as a new bug.

---

## 2. Required Information

Every bug bead MUST include the following fields at creation time. Incomplete filings waste triage effort and delay fixes.

### 2.1 Mandatory Fields

| Field | Description | Example |
|-------|-------------|---------|
| **Title** | One-line summary: `<subsystem>: <symptom>` | `db/search_v3: FTS query returns stale results after DELETE` |
| **Severity** | P0 (blocking), P1 (high), P2 (medium), P3 (low) | P1 |
| **Subsystem** | Crate or module where the bug lives | `mcp-agent-mail-db/search_v3.rs` |
| **Repro** | Minimal reproduction steps or test name | `cargo test -p mcp-agent-mail-db search_stale -- --nocapture` |
| **Observed behavior** | What actually happens | `Returns 3 rows; expected 2 after deletion` |
| **Expected behavior** | What should happen | `Deleted message should not appear in FTS results` |
| **Discovery context** | How the bug was found | `Audit pass 2 on DB subsystem (br-97gc6.5)` |

### 2.2 Optional but Recommended

| Field | When to Include |
|-------|-----------------|
| **Root cause hypothesis** | When the auditor has a theory |
| **Affected tests** | List of tests that should catch this but do not |
| **Related beads** | Cross-references to related bugs or audit lanes |
| **Fix sketch** | Brief description of the likely fix approach |

---

## 3. Triage Process

### 3.1 Severity Classification

| Severity | Definition | SLA |
|----------|-----------|-----|
| **P0 - Blocking** | Data loss, security violation, crash on reachable path, or CI gate failure. Prevents release or blocks other audit lanes. | Must be addressed in the current session or explicitly escalated. |
| **P1 - High** | Incorrect behavior with user-visible impact. Supported compatibility-contract regression. Potential data integrity issue under edge conditions. | Must be addressed before the subsystem audit lane can close. |
| **P2 - Medium** | Misleading diagnostics, documentation drift, non-critical error handling gaps. No data loss but degrades operator experience. | Should be addressed during the audit wave. May be deferred to next wave with justification. |
| **P3 - Low** | Style, minor ergonomic issues, theoretical edge cases with no known trigger. | Track for future cleanup. May be closed as "won't fix" with documented rationale. |

### 3.2 Assignment Rules

1. **Parent bead**: Every bug bead MUST be linked as a child of the appropriate subsystem audit lane (br-97gc6.4 through br-97gc6.7) or the blocker lane (br-97gc6.3).
2. **Owner**: The agent that discovers the bug files it. Ownership transfers to the fixing agent when work begins.
3. **Labels**: Apply at minimum: the subsystem label (e.g., `db`, `core`, `server`) and `bug`. Add `regression` if the behavior previously worked. Add `parity` if it is a Python-Rust deviation.
4. **Deduplication**: Before filing, search existing beads (`br list --label bug`) for the same symptom. If a duplicate exists, add a comment to the existing bead rather than creating a new one.

### 3.3 Escalation

- A P2 bug that is discovered to have data-loss potential MUST be re-triaged to P0/P1.
- A bug that blocks two or more subsystem audit lanes MUST be moved to the blocker lane (br-97gc6.3) regardless of original severity.
- External dependency bugs (frankensqlite, sqlmodel_rust, etc.) go to the blocker ledger (br-97gc6.3.3) with a note on whether a workaround exists.

---

## 4. Closure Criteria

A bug bead may be moved to `closed` status ONLY when ALL of the following conditions are met.

### 4.1 Required Closure Evidence

| Criterion | Evidence Required |
|-----------|-------------------|
| **Fix committed** | A commit SHA that addresses the root cause is referenced in the bead. |
| **Test backfill** | At least one new or modified test exercises the exact failure mode. The test MUST fail on the pre-fix code and pass on the post-fix code. |
| **No regressions** | `cargo test --workspace` passes (or rch equivalent). No new test failures introduced by the fix. |
| **Audit lane updated** | The parent subsystem audit bead reflects the bug as remediated in its pass notes. |

### 4.2 Acceptable Closure Without Full Fix

In specific circumstances, a bug may be closed without a code fix:

| Disposition | When Allowed | Required Evidence |
|-------------|-------------|-------------------|
| **Won't fix** | P3 only. The fix cost exceeds the impact, and the behavior is documented as a known limitation. | Written rationale in the bead. Approval from the audit lane owner. |
| **Duplicate** | Another bead already tracks the same root cause. | Link to the canonical bead. |
| **Not reproducible** | Multiple attempts to reproduce fail, and the original reporter confirms. | Reproduction attempts documented. At least one defensive test added if the failure mode is plausible. |
| **External dependency** | The root cause is in a crate outside this workspace and cannot be worked around. | Blocker ledger entry (br-97gc6.3.3). Upstream issue link if applicable. |
| **False alarm** | Investigation confirms the behavior is correct. | Analysis notes in the bead explaining why the original report was incorrect. |

### 4.3 Closure Anti-Patterns (Do NOT Close When)

- The fix is committed but no test covers the failure mode ("it works on my machine").
- The test passes but only because it tests a different code path than the one that was broken.
- The fix is a workaround that masks the symptom without addressing the root cause (unless explicitly marked as `workaround` with a follow-up bead for the real fix).
- The reporter has not confirmed the fix (for bugs reported by a different agent than the fixer).

---

## 5. Reopen Rules

A closed bug MUST be reopened (`br update <id> --status open`) when any of the following occur:

### 5.1 Mandatory Reopen Triggers

| Trigger | Action |
|---------|--------|
| **Regression** | The test backfill for this bug starts failing again. Reopen immediately, escalate to P0. |
| **Incomplete fix** | A new audit pass discovers the same symptom on a code path not covered by the original fix. |
| **Fix reverted** | The commit containing the fix is reverted (intentionally or as part of a larger revert). |
| **New evidence** | Information surfaces that the original closure evidence was incorrect or insufficient (e.g., the test was not actually exercising the buggy path). |
| **Scope expansion** | The original bug is found to affect additional subsystems not covered by the initial fix and test. |

### 5.2 Reopen Process

1. Set status back to `open`.
2. Add a comment explaining WHY the bug is being reopened, with a link to the evidence (failing test, new audit finding, revert commit).
3. Bump severity if the reopen reveals the bug is more impactful than originally assessed.
4. Re-link to the active audit lane if the original lane is already closed (create a new child under br-97gc6.8 convergence if needed).

### 5.3 Do NOT Reopen When

- A **related but distinct** bug is found -- file a new bead instead.
- The behavior changed due to a **deliberate design change** that supersedes the original bug's expected behavior.
- The test backfill is failing due to an **unrelated infrastructure issue** (flaky CI, rch contention) -- fix the infra issue instead.

---

## 6. Integration with the Beads Issue Tracker

### 6.1 Bead Lifecycle for Bugs

```
Discovery -> File bead (open) -> Triage (assign severity, lane, owner)
         -> Investigation (in_progress) -> Fix + Test -> Verification
         -> Close (closed) -- or -- Reopen (open) if criteria in Section 5 are met
```

### 6.2 Naming Convention

Bug beads follow the pattern: `<subsystem>: <concise symptom description>`

Examples:
- `core/timestamps: saturating_sub missing on negative delta`
- `db/integrity: rebuild_inbox_stats races with concurrent writes`
- `server/auth: token refresh returns 401 instead of re-prompting`

### 6.3 Cross-References

- Every bug bead MUST have a `parent-child` dependency on its subsystem audit lane.
- Bugs that block other beads MUST use `blocks` dependency, not just a comment.
- Bugs discovered during a specific audit pass SHOULD include the pass number in their discovery context field (e.g., "pass 2, map phase").

### 6.4 Residual Risk Register

Bugs closed as "won't fix" or "external dependency" MUST be referenced in the residual risk register (br-97gc6.8) so the convergence bead has an explicit inventory of accepted risks.

---

## 7. Audit Pass Integration

This policy is used by every subsystem audit lane (br-97gc6.4 through br-97gc6.7). Each lane's pass structure is:

1. **Map** -- inventory code paths and existing coverage.
2. **Audit** -- systematic review; file bugs per this policy.
3. **Fix** -- address bugs; provide closure evidence per Section 4.
4. **Rescan** -- re-audit fixed areas; reopen per Section 5 if needed.
5. **Verify** -- confirm all bugs in the lane are closed or explicitly deferred.

Bugs found during the Rescan or Verify phases that are NEW (not reopens) follow the same intake rules in Section 1 and are filed as new beads, not comments on existing ones.
