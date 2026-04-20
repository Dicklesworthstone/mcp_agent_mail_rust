# Operator Verification Runbook — Git 2.51.0 Fix

**Epic:** br-8ujfs (Git 2.51.0 concurrency hardening)
**Bead:** br-8ujfs.8.2 (H2)
**Audience:** ops running mcp-agent-mail on any box

This is a ~10-minute verification procedure for confirming the git
2.51.0 hardening is working on your box. Run each step in order.
If any step FAILs, attach the named artifact + your box's
`uname -a` + `git --version` output to a bug report.

---

## Pre-check (30 seconds)

```bash
am doctor check --format json \
  | jq '.checks[] | select(.check | startswith("git_binary_"))'
```

Expect one or two JSON objects (one `git_binary_path`, optionally
one `git_binary_env` if `AM_GIT_BINARY` is set). Each should have
`"status": "ok"` unless the resolved binary is on the known-bad list.

**FAIL signal:** a `"status": "fail"` entry with
`"code": "GIT_2_51_0_INDEX_RACE"`. Remediation: set `AM_GIT_BINARY`
or upgrade/downgrade system git. See `RECOVERY_RUNBOOK.md`.

---

## Baseline kernel log check (30 seconds)

```bash
journalctl -k --since '1 hour ago' \
  | awk '/segfault/ && /git\[/' \
  | wc -l
```

Expect: `0` in the last hour of a running system. If you see
non-zero, run step 4 ("apply"). If you just installed and haven't
exercised the server, re-run this after a representative session.

---

## Synthetic stress test (5-7 minutes)

Requires the `git_251_racer` fixture:

```bash
cargo test -p mcp-agent-mail-storage \
  --test libgit2_index_race_immunity \
  -- --nocapture
```

The test is gated by `AM_C8_RUN=1`. On a healthy box the libgit2
immunity assertion holds (zero reader panics). Check-box:

- `Verdict: Immune` or `ImmuneWithGracefulErrors` in the
  `[C8 REPORT]` section of stderr.
- Exit code 0.

**FAIL signal:** `Verdict: Racy` — the libgit2 reader panicked
mid-walk. This would contradict the core premise of Track C and
should be escalated immediately.

---

## Ledger / analytics sanity (30 seconds)

```bash
# Count retries in the evidence ledger (requires a running session)
am robot analytics --format json --since 1h \
  | jq '.git_instability // empty'
```

Expect: `null` or `{"exhausted_retries": 0}` on a healthy box.

**FAIL signal:** `"exhausted_retries"` > 0. This means retries
ran out of budget and the operator has an unresolved problem.

---

## fix-orphan-refs dry-run (1-2 minutes)

Surveys every registered project for orphan refs:

```bash
am doctor fix-orphan-refs --all --dry-run --format json \
  | jq '{findings: [.projects[] | .summary.findings] | add,
         projects: .summary.total_projects}'
```

Expect: `{"findings": 0, "projects": N}` where N matches your
registered project count.

**FAIL signal:** `"findings" > 0`. Existing damage needs cleanup:

```bash
# Review what would be pruned
am doctor fix-orphan-refs --all --dry-run --format human

# If safe, apply
am doctor fix-orphan-refs --all --apply
```

Backups land in `<STORAGE_ROOT>/backups/refs/<project_slug>/` —
keep the last 10 per project. Inspect before you `--apply` if in
doubt.

---

## Linter sanity (10 seconds)

```bash
bash scripts/lint_git_shellouts.sh
```

Expect: `OK — no new unwrapped git shell-outs in production code.`
with exit 0.

**FAIL signal:** nonzero exit with a list of offending sites.
Usually indicates an unrelated PR added a raw `Command::new("git")`
that needs to be routed through `GitCmd`.

---

## Wrapper script (optional, 30 seconds)

If you use `git-with-amlock` for external tooling:

```bash
# Smoke: status inside a known repo
git-with-amlock status --short

# Timeout path: hold the flock externally, verify wrapper aborts
flock -x /path/to/repo/.git/am.git-serialize.lock \
      sleep 70 &
lock_pid=$!
time git-with-amlock status
# Should exit 75 (EX_TEMPFAIL) in ~60s; then:
kill $lock_pid
```

---

## Report form

Copy-paste this block when filing a verification report:

```
=== mcp-agent-mail git 2.51.0 verification ===
Date: YYYY-MM-DDTHH:MM:SSZ
Box:  $(uname -a)
Git:  $(git --version) [AM_GIT_BINARY=$AM_GIT_BINARY]
Am:   $(am --version 2>/dev/null || echo unknown)

[step 1 pre-check]  FAIL/PASS — paste jq output
[step 2 kernel log] FAIL/PASS — <count> segfaults in last hour
[step 3 stress]     FAIL/PASS — <verdict> <ops_total>
[step 4 ledger]     FAIL/PASS — <exhausted>
[step 5 refs]       FAIL/PASS — <findings> across <projects>
[step 6 lint]       FAIL/PASS — exit <code>
[step 7 wrapper]    SKIP/PASS/FAIL

Attachments (if any): path to tests/artifacts/libgit2_immunity/...
                      kernel log excerpt
                      am doctor check output
```

## When a step fails

Each FAIL signal above has a named remediation. In order of
likelihood:

1. Set `AM_GIT_BINARY=/path/to/git-2.50.x/bin/git` (covers 90% of
   failure modes on 2.51.0 boxes).
2. Run `am doctor fix-orphan-refs --all --apply` to clean up ref
   damage.
3. Consult `RECOVERY_RUNBOOK.md#git-2-51-0-index-race`.
4. If the immunity test (`step 3`) fails specifically, escalate:
   attach the `tests/artifacts/libgit2_immunity/<ts>/report.json`
   to a bug report.
