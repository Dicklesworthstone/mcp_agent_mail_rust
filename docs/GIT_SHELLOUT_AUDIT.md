# Git Shell-Out Audit — br-8ujfs.3.1 (C1)

**Generated:** 2026-04-19
**Auditor:** RubyKnoll
**Epic:** br-8ujfs (Git 2.51.0 concurrency hardening)
**Total sites audited:** 45 (`rg -n 'Command::new\("git"\)' crates/`)

## Purpose

Ground-truth inventory of every in-process `Command::new("git")` site
in the workspace, classified by migration strategy. This document is
the source of truth that Tracks C, D, and E consume to decide per
site whether to MIGRATE to libgit2, WRAP in `run_git_locked`, keep
as-is (guard), or leave in place (test fixtures).

## Classification key

| Code | Meaning | Action |
|------|---------|--------|
| `MIGRATE` | Replace with libgit2 equivalent | Track C (C2..C6) |
| `WRAP` | Keep CLI invocation, route through `run_git_locked` | Track D (D1) |
| `KEEP_GUARD` | Inside user's pre-commit process, same pid | Track E (E5) only — no WRAP, no MIGRATE |
| `TEST_FIXTURE` | `#[cfg(test)]` / `tests/`; exercises real git deliberately | SIGSEGV-retry optional; no WRAP |
| `DELETE` | Dead code / redundant path covered by libgit2 fallback | Delete the block |

## Decision rules (from parent bead)

1. **Same process as the git under test** → `KEEP_GUARD` (E5 only).
   Rationale: guard's pre-commit hook runs inside the git commit's
   own pid. Wrapping with flock would deadlock.
2. **libgit2 covers the operation cleanly** → `MIGRATE`.
3. **libgit2 does not cover it, or coverage requires non-trivial
   refactor during a reliability fix** → `WRAP`.
4. **Test-only helper** → `TEST_FIXTURE`; optionally add SIGSEGV
   retry locally if the test is used in CI on 2.51.0 runners.
5. **Redundant code path** → `DELETE`.

## Full site table

### Production sites (23)

| # | Site | Operation | Class | Owner bead | Rationale |
|---|------|-----------|-------|------------|-----------|
| 1 | `crates/mcp-agent-mail-core/src/identity.rs:168` | `git_cmd(repo, args)` — generic helper for `git config`, `git rev-parse`, `git remote get-url` | MIGRATE | C4 | libgit2 has clean equivalents: `Config`, `head()`, `find_remote`. Called at registration time under parallel load. |
| 2 | `crates/mcp-agent-mail-storage/src/lib.rs:7341` | `git -C <workdir> read-tree HEAD` | DELETE | C5 | Immediately falls through to libgit2 `reset_index_to_head` path. Redundant; removing it kills a race surface. |
| 3 | `crates/mcp-agent-mail-tools/src/resources.rs:3784` | `git -C <repo> log -1 --format=%ct -- <pathspecs...>` (inside `reservation_git_latest_activity_micros`) | MIGRATE | C2 | Hot path via `force_release_file_reservation`. libgit2 `Revwalk` + `Pathspec::match_tree` equivalent. |
| 4 | `crates/mcp-agent-mail-tools/src/resources.rs:3906` | `git -C <repo> ls-files -c -o --exclude-standard -- :(glob)<spec>` | MIGRATE | C3 | libgit2 `statuses()` with `include_untracked` + `include_ignored=false` matches. |
| 5 | `crates/mcp-agent-mail-tools/src/resources.rs:4005` | test helper `run_git` in `reservation_activity_tests` | TEST_FIXTURE | — | Inside `#[cfg(test)] mod reservation_activity_tests`. |
| 6 | `crates/mcp-agent-mail-share/src/hosting.rs:236` | `git remote get-url origin` in `git_remote_url` | MIGRATE | C6 | libgit2 `find_remote("origin")?.url()`. |
| 7 | `crates/mcp-agent-mail-share/src/detection.rs:532` | Same as #6 (identical helper in a different crate) | MIGRATE | C6 | Same libgit2 substitute. Consolidate into a shared helper in `mcp-agent-mail-core` as part of migration. |
| 8 | `crates/mcp-agent-mail-server/src/cleanup.rs:554` | `git -C <workspace> ls-files -c -o --exclude-standard -- <pathspec>` with streaming pipe (inside `check_git_listed_activity`) | WRAP | D1 | Uses stdout streaming + mid-walk kill on match; libgit2 port requires more invasive refactor. Wrap for now; migrate in follow-up. |
| 9 | `crates/mcp-agent-mail-server/src/cleanup.rs:777` | `git -C <workspace> rev-parse HEAD` in `git_head_oid_for_workspace` | MIGRATE | C6 (grouped with share) | Trivial libgit2 replacement: `repo.head()?.peel_to_commit()?.id()`. |
| 10 | `crates/mcp-agent-mail-server/src/cleanup.rs:808` | `git -C <workspace> log -1 --format=%ct -- <pathspec>` in `git_latest_commit_us` | MIGRATE | C2 (grouped) | Same op as #3; share implementation via C2's new helper. |
| 11 | `crates/mcp-agent-mail-cli/src/lib.rs:13456` | `git_output_text(cwd, args)` — generic helper, used by `git_repo_root` etc. | MIGRATE | C4 (grouped) | Same pattern as identity.rs `git_cmd`. Consolidate with C4's new helper. |
| 12 | `crates/mcp-agent-mail-cli/src/lib.rs:13639` | `git -C <repo_root> add <paths>` (inside `git_add_and_commit` — `projects mark-identity` flow) | WRAP | D1 | libgit2 can stage files, but we need to match CLI behavior (respect .gitattributes, clean filters) exactly because this commits user-visible content. Wrap to protect; consider MIGRATE in follow-up epic. |
| 13 | `crates/mcp-agent-mail-cli/src/lib.rs:13656` | `git -C <repo_root> commit -m <msg>` (pairs with #12) | WRAP | D1 | Same rationale as #12. |
| 14 | `crates/mcp-agent-mail-cli/src/lib.rs:44962` | `git -C <path> rev-parse --abbrev-ref HEAD` in `compute_git_branch` | MIGRATE | C4 (grouped) | `repo.head()?.shorthand()` equivalent. |
| 15 | `crates/mcp-agent-mail-cli/src/e2e_artifacts.rs:201` | `git rev-parse HEAD` inside `GitInfo::capture` (cwd-based; called from E2E test harness) | WRAP | D1 | Runs in CI during E2E runs; wrap for resilience. Operator-invoked dev tool too. |
| 16 | `crates/mcp-agent-mail-cli/src/e2e_artifacts.rs:216` | `git rev-parse --abbrev-ref HEAD` | WRAP | D1 | Same harness. |
| 17 | `crates/mcp-agent-mail-cli/src/e2e_artifacts.rs:231` | `git status --porcelain` | WRAP | D1 | Same harness. |
| 18 | `crates/mcp-agent-mail-guard/src/lib.rs:1571` | `git diff --cached --name-status -M -z` in `get_staged_paths` | KEEP_GUARD | E5 | Runs inside user's pre-commit hook; same pid; cannot deadlock. |
| 19 | `crates/mcp-agent-mail-guard/src/lib.rs:1610` | `git rev-list --topo-order <range>` in `get_push_paths` | KEEP_GUARD | E5 | Pre-push hook; same rationale. |
| 20 | `crates/mcp-agent-mail-guard/src/lib.rs:1633` | `git diff-tree --root -r --no-commit-id --name-status -M --no-ext-diff --diff-filter=ACMRDTU -z -m <sha>` | KEEP_GUARD | E5 | Per-commit path enumeration in pre-push. |
| 21 | `crates/mcp-agent-mail-guard/src/lib.rs:1664` | Fallback `git diff --name-status -M -z <range>` | KEEP_GUARD | E5 | Diff fallback in pre-push. |
| 22 | `crates/mcp-agent-mail-guard/src/lib.rs:1785` | test helper `run_git` (inside `mod tests`) | TEST_FIXTURE | — | Already inside `#[cfg(test)]`. |
| 23 | `crates/mcp-agent-mail-guard/src/lib.rs:1807` | test helper `run_git_stdout` | TEST_FIXTURE | — | Same. |

### Test-only sites (22)

| # | Site | Operation | Class |
|---|------|-----------|-------|
| 24 | `crates/mcp-agent-mail-cli/tests/integration_runs.rs:311` | test setup git | TEST_FIXTURE |
| 25 | `crates/mcp-agent-mail-cli/tests/integration_runs.rs:526` | test setup git | TEST_FIXTURE |
| 26 | `crates/mcp-agent-mail-cli/tests/integration_runs.rs:758` | test log inspection | TEST_FIXTURE |
| 27 | `crates/mcp-agent-mail-cli/tests/cli_json_snapshots.rs:195` | `git init` in test | TEST_FIXTURE |
| 28 | `crates/mcp-agent-mail-conformance/tests/conformance.rs:711` | conformance fixture | TEST_FIXTURE |
| 29-30 | `crates/mcp-agent-mail-conformance/tests/contact_enforcement_outage.rs:137,449` | conformance fixtures | TEST_FIXTURE |
| 31-34 | `crates/mcp-agent-mail-cli/src/lib.rs:38757, 38864, 38952, 41615` | inside `mod tests` (starts at line 23631) | TEST_FIXTURE |
| 35-43 | `crates/mcp-agent-mail-server/src/cleanup.rs:1444, 1452, 1460, 1468, 1476, 1502, 1509, 1516, 1523, 1530` | inside `mod tests` (starts at line 975) | TEST_FIXTURE |
| 44 | `crates/mcp-agent-mail-server/src/tui_screens/timeline.rs:2800` | inside test `run_git` helper | TEST_FIXTURE |

## Dynamic `Command::new($X)` check (C1 v3 revision)

Ran:

```
ast-grep run -l Rust -p 'Command::new($X)' crates/
```

All non-literal `$X` hits use `Command::new(binary_name)` where
`binary_name` is a local variable captured from:
- `std::env::var("WHATEVER")` for non-git subprocesses (rustc/cargo) —
  out of scope.
- Test helpers that resolve path (test-only, out of scope).

**No dynamic `Command::new(git_path)` sites exist today.** All
sites above are literal `"git"` strings.

## Hook template audit (C1 v3 revision)

Searched `crates/mcp-agent-mail-guard/src/install.rs` for string
literals that spawn git:

```
rg -n 'subprocess\.run|os\.system' crates/mcp-agent-mail-guard/src/
```

The hook template emits a Python script that calls `subprocess.run`
against `git`. This is in scope for E5's Python-side retry wrapper.

Specific template lines (line numbers will shift as E5 work lands):

```
subprocess.check_output(["git", "diff", "--cached", "--name-status", ...])
subprocess.check_output(["git", "rev-list", ...])
subprocess.check_output(["git", "diff-tree", ...])
```

These get the `_run_git_with_retry` wrapper from E5.

## Summary

| Classification | Count | Owner |
|----------------|-------|-------|
| MIGRATE | 8 | C2 (2), C3 (1), C4 (4 consolidated), C5 (1), C6 (3 consolidated) |
| WRAP | 7 | D1 |
| KEEP_GUARD | 4 | E5 |
| TEST_FIXTURE | 26 | none (may opt into E1/E2 locally) |
| DELETE | 1 | C5 |

**Workflow impact:**
- After C2..C6 MIGRATEs (8 sites) complete, 8 shell-outs disappear.
- After D1 WRAPs (7 sites) complete, 7 shell-outs go through
  `run_git_locked`.
- After C5 DELETE (1 site) completes, 1 shell-out disappears.
- Remainder (22 test + 4 guard = 26) stays as direct `Command::new`
  by design.

## Decision matrix for new sites

When adding a new `Command::new("git")`:

```
Will this run inside a git hook subprocess?
  YES → KEEP_GUARD; add SIGSEGV retry via E5's helper if needed.
  NO  → Is this test-only (#[cfg(test)] / tests/*)?
         YES → TEST_FIXTURE; direct Command::new is OK.
         NO  → Does libgit2 cover the operation with equivalent semantics?
                YES → MIGRATE; use `git2::Repository` directly.
                NO  → WRAP; use `mcp_agent_mail_core::git_lock::GitCmd`.
```

The D2 CI lint enforces the NO→NO case (production non-guard
sites MUST use GitCmd).

## Ongoing enforcement

- `scripts/count_git_shellouts.sh` (D2 will create) prints a
  per-crate breakdown; the baseline values live in
  `docs/GIT_SHELLOUT_AUDIT_COUNT.txt` and drift must be explained.
- Baseline counts committed alongside this audit:

```
Production MIGRATE candidates : 8 (Track C completion target: 0)
Production WRAP candidates    : 7 (Track D completion target: 0 direct; 7 via GitCmd)
Production KEEP_GUARD         : 4 (Track E wraps with retry only)
Production DELETE             : 1 (Track C5 completion target: 0)
Test fixtures                 : 26 (no target; these stay)
Dynamic bindings              : 0 (no target; none exist)
```

## Links

- Epic: br-8ujfs
- This bead: br-8ujfs.3.1 (C1)
- Migration beads: br-8ujfs.3.2..3.8 (C2..C8), br-8ujfs.4.1 (D1),
  br-8ujfs.4.2 (D2), br-8ujfs.5.5 (E5)
- Research: br-8ujfs.1.1 (A1) / `docs/GIT_251_FINDINGS.md`
- Design note: br-8ujfs.2.1 (B1) / `docs/DESIGN_git_lock.md`
