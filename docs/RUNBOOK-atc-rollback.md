# ATC Learning Rollback Runbook

Procedures for rolling back ATC learning when production issues arise.
Each scenario includes detection criteria, step-by-step rollback, verification,
and post-incident guidance.

**Prerequisites:** Familiarity with `AM_ATC_WRITE_MODE`, `ATC_LEARNING_DISABLED`,
and the `.atc_kill_switch` sentinel file. See
[OPERATOR_RUNBOOK.md](OPERATOR_RUNBOOK.md#emergency-disable-atc-learning) for
the kill-switch reference.

---

## Scenario 1: Derivation Bug — Bad Data Entering the Ledger

### Detection

- `atc_derivation_errors_total` counter rising (check via `am robot atc --toon`)
- Manual spot-check of recent experience rows reveals incorrect feature vectors
  or misattributed event kinds
- Downstream policy outputs shift without corresponding agent-behavior changes

### Rollback procedure

```bash
# 1. Activate kill switch (immediate, no restart)
echo "derivation bug — bad feature vectors $(date -u +%FT%TZ)" \
  > ~/.mcp_agent_mail_git_mailbox_repo/.atc_kill_switch

# 2. Verify writes stopped (wait one ATC tick, ~5 sec)
am robot atc --toon | grep -E 'kill_switch|effective_mode'
# Expected: kill_switch.active = true, effective_mode = "off"

# 3. Identify the bad-data window
#    Check the last N experience rows for anomalies:
sqlite3 ~/.mcp_agent_mail_git_mailbox_repo/agent_mail.sqlite3 \
  "SELECT id, event_kind, created_at FROM atc_experiences ORDER BY created_at DESC LIMIT 50;"
```

**Expected time-to-rollback:** < 10 seconds.

### Post-incident

- Do NOT fix rows in place — the append-only ledger invariant must hold.
- Retention compaction will age out bad rows naturally
  (`RESOLVED_EXPERIENCE_DROP_AFTER_DAYS`).
- If immediate cleanup is needed, write a targeted `DELETE` for the known-bad
  `created_at` range, then run `VACUUM`.
- File a bead for the root cause before re-enabling.

---

## Scenario 2: Performance Regression — send_message Latency Above Budget

### Detection

- `atc.insert_experience` span p95 latency exceeds 500 microseconds
- `send_message` tool p95 exceeds 5% over baseline (check via
  `am robot metrics --toon` or TUI Metrics screen)
- Users or agents report sluggish message delivery

### Rollback procedure

```bash
# Step 1: Try shadow mode first (lighter weight, preserves trace data)
export AM_ATC_WRITE_MODE=shadow
# Restart the server or set via runtime config

# Step 2: Verify latency returns to baseline
am robot metrics --toon | grep send_message
# If p95 is still elevated after 2 minutes in shadow mode:

# Step 3: Full kill switch
echo "perf regression — send_message p95 above budget $(date -u +%FT%TZ)" \
  > ~/.mcp_agent_mail_git_mailbox_repo/.atc_kill_switch
```

**Expected time-to-rollback:** < 30 seconds (shadow), < 10 seconds (kill switch).

### Post-incident

- File a perf bead with flamegraph artifacts from the regression window.
- Do not resume `live` writes until the fix is deployed AND shadow-mode trace
  data confirms the regression is resolved under production load.
- When resuming, follow the restoration procedure at the bottom of this document.

---

## Scenario 3: DB Corruption — atc_experiences Table Inconsistent

### Detection

- `am doctor` or `br doctor` checks fail on ATC tables
- Rollup counts (`atc_experience_rollups`) diverge from raw row counts
- Queries against `atc_experiences` return unexpected errors
  (`malformed database`, `disk I/O error`, column-type mismatches)

### Rollback procedure

```bash
# 1. Kill switch immediately
echo "db corruption — atc tables inconsistent $(date -u +%FT%TZ)" \
  > ~/.mcp_agent_mail_git_mailbox_repo/.atc_kill_switch

# 2. Take a backup before any repair
cp ~/.mcp_agent_mail_git_mailbox_repo/agent_mail.sqlite3 \
   ~/.mcp_agent_mail_git_mailbox_repo/agent_mail.sqlite3.pre-repair-$(date +%s)

# 3. Option A: Aggressive retention compaction (preserves good data)
sqlite3 ~/.mcp_agent_mail_git_mailbox_repo/agent_mail.sqlite3 \
  "DELETE FROM atc_experience_rollups; DELETE FROM atc_experiences WHERE state = 'resolved';"

# 3. Option B: Full truncate (loses all learning data, schema preserved)
sqlite3 ~/.mcp_agent_mail_git_mailbox_repo/agent_mail.sqlite3 \
  "DELETE FROM atc_experience_rollups; DELETE FROM atc_experiences;"

# 4. Rebuild integrity
sqlite3 ~/.mcp_agent_mail_git_mailbox_repo/agent_mail.sqlite3 "PRAGMA integrity_check;"
# Must return "ok"

# 5. VACUUM to reclaim space
sqlite3 ~/.mcp_agent_mail_git_mailbox_repo/agent_mail.sqlite3 "VACUUM;"
```

**Expected time-to-rollback:** < 1 minute (kill switch + backup), 1-5 minutes
(truncate + integrity check).

### Post-incident

- Investigate root cause: concurrent write bug, frankensqlite edge case,
  disk failure, or interrupted migration.
- Do not re-enable until the cause is identified and fixed.
- When resuming, start from shadow mode for at least 24 hours.

---

## Scenario 4: Schema Migration Failure — Partial v16 to v17 Upgrade

### Detection

- Queries fail with `no such column`, `no such table`, or `table already exists`
  immediately after a version upgrade
- Server startup logs show migration errors
- `am doctor` reports schema version mismatch

### Rollback procedure

```bash
# 1. Stop the server
# (systemctl stop mcp-agent-mail, or Ctrl+C)

# 2. Kill switch (prevents writes on restart)
echo "schema migration failure $(date -u +%FT%TZ)" \
  > ~/.mcp_agent_mail_git_mailbox_repo/.atc_kill_switch

# 3. Restore pre-migration backup
#    If automatic backup exists:
cp ~/.mcp_agent_mail_git_mailbox_repo/agent_mail.sqlite3.pre-migration \
   ~/.mcp_agent_mail_git_mailbox_repo/agent_mail.sqlite3

#    If no backup, reconstruct from Git archive:
am doctor --reconstruct-from-archive

# 4. Downgrade binary to previous version
#    (use the previous release binary or git checkout the prior tag)

# 5. Restart with the old binary + kill switch active
am serve-http
```

**Expected time-to-rollback:** 1-5 minutes (restore + restart).

### Post-incident

- Never re-run a failed migration without: (a) a verified backup, (b) a dry-run
  on a copy of the production DB, (c) the migration script reviewed for
  idempotency.
- File a bead with the exact error and the DB state at failure time.

---

## Scenario 5: Policy Engine Producing Bad Decisions

### Detection

- Manual review of `atc.decision` tracing spans reveals nonsensical outputs
  (e.g., releasing reservations for active agents, sending advisories to
  non-existent agents)
- Operators observe ATC actions that contradict known agent state
- `am robot atc --summary` shows high regret or anomalous decision counts

### Rollback procedure

```bash
# This scenario does NOT require a kill switch — the ledger is fine,
# only the decision engine is misbehaving.

# Option A: Force safe mode (disables proactive actions, keeps observation)
export AM_ATC_SAFE_MODE=1
# Restart or wait for the runtime config to take effect

# Option B: If safe mode is insufficient, disable the operator runtime
export AM_ATC_ENABLED=false
# Restart the server — ATC observation continues but no operator actions

# Option C: Nuclear — full kill switch (stops all ATC activity)
echo "bad policy decisions $(date -u +%FT%TZ)" \
  > ~/.mcp_agent_mail_git_mailbox_repo/.atc_kill_switch
```

**Expected time-to-rollback:** < 10 seconds (safe mode or disable).

### Post-incident

- Capture the decision trace spans and the policy bundle that produced them.
- The ledger data is not at fault — do not truncate experience rows.
- Fix the policy logic, validate against the captured traces, then re-enable.
- This scenario is independent of ledger integrity; learning can continue
  even while the decision engine is disabled.

---

## Restoration Procedure (After Any Rollback)

After resolving the root cause, re-enable ATC learning following the Seam 3.0
canary process:

1. **Identify and fix** the root cause. Commit the fix with a test that
   reproduces the original failure.

2. **Deploy to shadow mode** for a minimum of 24 hours:
   ```bash
   export AM_ATC_WRITE_MODE=shadow
   rm ~/.mcp_agent_mail_git_mailbox_repo/.atc_kill_switch
   # Restart the server
   ```

3. **Analyze shadow data** for the specific failure class that triggered the
   rollback. Verify via TRACE-level logs that the would-insert payloads are
   correct.

4. **Promote to live** if shadow data is clean:
   ```bash
   export AM_ATC_WRITE_MODE=live
   # Restart the server
   ```

5. **Retain kill-switch readiness** for 1 week post-resume. Monitor the
   detection criteria from the original scenario during this period.

6. **Accept the gap.** The ledger is append-only. Events missed during the
   outage are not backfilled. Gaps are expected and documented; the retention
   and rollup logic handles sparse data correctly.

---

## Fire Drill Checklist

Run quarterly to verify procedures remain current:

- [ ] Simulate Scenario 1: inject a bad experience row, verify detection + kill switch
- [ ] Simulate Scenario 2: inject artificial latency, verify shadow fallback
- [ ] Simulate Scenario 3: corrupt a rollup row, verify doctor detection + truncate recovery
- [ ] Simulate Scenario 5: feed bad features to the policy engine, verify safe-mode path
- [ ] Time each drill — record time-to-rollback in the incident log
- [ ] Verify all copy-paste commands in this runbook still work against current schema
- [ ] Update this document if any procedure has changed
