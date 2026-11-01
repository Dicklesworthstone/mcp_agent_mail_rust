# Recovery Runbook

## Symptoms
- SQLite corruption detected (`PRAGMA integrity_check` fails).
- Search/index inconsistencies vs archive.
- Missing records in SQLite after archive write.

## Steps
1. Acquire project archive lock (`projects/<slug>/.archive.lock`).
2. Validate archive as source of truth (ensure files exist, git repo healthy).
3. Rebuild SQLite index from archive contents.
4. Update version markers (archive commit hash + db schema version).
5. Verify counts and sample hashes.
6. Release lock.

## Commands (Planned)
- `reindex` (archive -> SQLite)
- `integrity-check` (SQLite + archive)
- `doctor repair` (wraps the above with backups)

## Metrics-index growth and memory exhaustion (GH#372)

FrankenSQLite 0.4.10 through 0.4.12 can leave very sparse B-tree pages when
inserting descending index keys. Agent Mail's metrics history uses
`idx_tool_metrics_snapshots_tool_ts(tool_name, collected_ts DESC)`: each new
sample sorts before the older samples for that tool. With metrics persistence
enabled, this can amplify both database size and the memory needed to open it.
`PRAGMA integrity_check` can still report `ok`, and `freelist_count` can be
small: these are allocated, sparsely populated index pages, not necessarily
corruption or free pages. See [issue #372](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/issues/372)
and [upstream #521](https://github.com/Dicklesworthstone/frankensqlite/issues/521).

**Upgrade first to a build using FrankenSQLite 0.4.13.** The dependency update
fixes subsequent insertions without changing the mailbox schema or rebuilding
existing files at startup. Existing oversized index pages need an explicit
offline rebuild; upgrading alone does not reclaim them. If an upgrade cannot
be deployed immediately, set `TOOL_METRICS_EMIT_ENABLED=false` in the server's
environment and restart it gracefully to stop new metrics snapshots. This is
a temporary growth mitigation, not a repair of the existing index.

### Offline recovery

Run every command with the same `DATABASE_URL` and `STORAGE_ROOT` as the
affected server. For a container deployment, stop the serving container and
run the upgraded `am` against the same persistent volume in an isolated
maintenance container. Do not start a second server on that volume.

1. Gracefully **stop all HTTP and stdio servers using the mailbox** and keep
   them stopped, including any supervisor that would restart them. For a
   systemd user service, for example:

   ```bash
   systemctl --user stop mcp-agent-mail
   am doctor locks --json
   am doctor drain --json
   ```

   `am doctor drain` only reports ownership; it does not stop anything. Require
   `safe_to_mutate=true`. A restart command is not a substitute for stopping
   the server throughout maintenance. Never remove activity locks to force
   admission. VACUUM rejects both `--allow-live-owner` and `--take-ownership`:
   replacing a wedged process's lock does not prove that the process stopped.

2. Keep an independent pre-incident backup outside the storage root when one
   is available, and ensure enough free space for a full backup and the
   temporary VACUUM image. Preview the selected database and page counts:

   ```bash
   am doctor vacuum --json
   ```

   Without `--yes`, this is read-only. `--dry-run` remains read-only even
   when combined with `--yes`. A clean integrity result does not rule out
   this specific bloat, so it does not eliminate the need for compaction.

3. Apply the offline rebuild:

   ```bash
   am doctor vacuum --yes --json
   ```

   The command holds the applicable mailbox activity locks exclusively from
   admission through verification, including the database lock, preventing a cooperating server from
   restarting in the middle. It first exports a full-schema, standalone
   backup that includes committed WAL rows, verifies its integrity and message
   contents, and flushes the backup to disk. The private
   `doctor-vacuum-backup-*/mailbox.sqlite3` directory beside the database is
   retained; the exact path is printed on stderr and returned as `backup_path`.
   Backup failure aborts before in-place VACUUM. It never reconstructs messages
   from the archive or drops metrics/history tables.

   Success requires a clean post-compaction integrity check and matching
   SHA-256 content witnesses for messages, recipient read/ack state, project
   deliveries/receipts, agents and projects. The witnesses include every
   stored column and are computed in bounded batches. The schema definitions,
   `user_version`, `application_id` and AUTOINCREMENT high-water marks must also
   survive unchanged. JSON reports
   `message_content_preserved=true`, before/after page counts and file sizes,
   and `reclaimed_pages`.

4. Confirm the command exited successfully, inspect the before/after sizes,
   and verify the mailbox before restarting the server:

   ```bash
   am doctor health
   am doctor check --json
   systemctl --user start mcp-agent-mail
   ```

   Use the applicable supervisor command for your deployment. After restart,
   confirm existing conversations and acknowledgements are readable and monitor
   memory/database growth with metrics enabled. The automatic maintenance
   cadence may still compact the separate ATC telemetry sidecar; main-mailbox
   compaction is reserved for this explicit offline command.

If compaction or content verification fails, **keep the server stopped** and
retain the reported backup and the original database family for diagnosis.
The command does not automatically restore over the mailbox. Do not copy a
backup over an active database or delete WAL/namespace sidecars by hand; use
the supported `am doctor restore <backup-path>` recovery surface only after
reviewing the failure and confirming offline ownership.

---

## Reconstruct/Promotion Refusal Loop (GH#208)

**Symptoms**: `serve-http` restarts forever under a supervisor; every
iteration logs `Startup detected mailbox database issues; reconstructing
from archive`, followed by `reconstruction succeeded but final database
promotion failed: recovery candidate ... would lose stable coordination
keys ... refusing promotion`, exit 1.

**Cause (fixed after v0.3.24)**: the promotion guard compared
full-payload fingerprints, so state the archive legitimately cannot
reproduce — registration tokens, per-recipient read/ack timestamps,
contact/reservation lifecycle fields, normalized message serialization —
counted as "loss" even when every coordination identity survived. One
drifted message additionally cascaded into all of its recipient rows.
Since the reconstruct trigger wanted `db ⊇ archive` while the guard
wanted `candidate ⊇ db` per payload byte, a mailbox drifted in both
directions could satisfy neither and the loop never terminated.

**Current behavior**: the guard refuses only on *identity* loss
(project slug + agent name; message stable key of
project/sender/thread/subject/created_ts; recipient identity chained to
the parent's identity digest; reservation lease key; contact
endpoints). Volatile payload drift is reported as informational counts
in the refusal text and the recovery receipt (`identity_*` fields), and
a candidate that is an identity superset promotes. Index-only
corruption (`wrong # of entries in index ...` from `integrity_check`)
is additionally repaired in place with `REINDEX` before any
reconstruction is attempted — including when `quick_check` alone looks
clean.

**On affected older versions (v0.3.22–v0.3.24)**:

```bash
systemctl --user stop agent-mail        # or your supervisor's stop
sqlite3 storage.sqlite3 "REINDEX;"      # heals index-only corruption losslessly
sqlite3 storage.sqlite3 "PRAGMA integrity_check;"   # expect: ok
systemctl --user start agent-mail
```

If the loop persists because live and archive genuinely diverged,
upgrade to a fixed build rather than hand-merging the candidate.

---

## Git 2.51.0 Index Race

**Symptoms** (any of):

- kernel log segfault lines with `ip 00000000001db250 ... error 4 in git[...]`
- `fatal: bad object HEAD` after an agent session
- orphan `refs/stash` whose target object is missing (ref points at a
  SHA that `git cat-file` can't find)
- `ahead=-1 behind=-1` from `ru status` or similar tooling
- repos that re-corrupt within minutes after a fresh `git clone`

### Diagnose

Check for the signature in kernel log:

```bash
journalctl -k --since '24 hours ago' \
  | awk '/segfault/ && /git\[/' \
  | head -20
```

A stream of `ip 00000000001db250 ... error 4` lines confirms this is
the 2.51.0 bug. (Disassemble `/usr/bin/git` around `0x1db240` if you
want to see the faulting `testb $0x1,0x52(%r12)` — it's reading
`cache_entry::ce_flags` after the backing mmap was invalidated.)

The doctor's known-bad-git detector gives a structured report:

```bash
am doctor fix --only fm-environment_toolchain-known-bad-git-no-override --list --json
```

### Remediate (in order)

1. **Point mcp-agent-mail at a safe git**
   (fastest mitigation; does NOT affect the rest of your system):

   ```bash
   # Identify a safe git binary somewhere on the system
   ls /usr/local/git-2.50*/bin/git 2>/dev/null
   ls /opt/git-2.50*/bin/git 2>/dev/null

   # Point mcp-agent-mail at it
   export AM_GIT_BINARY=/usr/local/git-2.50.2/bin/git
   # Persist in your shell profile; add to systemd unit if running as service.
   ```

2. **Upgrade or downgrade the system git**
   (preferred long-term — also protects unwrapped git calls like
   your IDE's `git commit`):

   - Ubuntu 25.10 (questing ships 2.51.0): wait for 2.51.1 in
     `questing-updates`, or downgrade via `apt install
     git=<older-version>`.
   - macOS Homebrew: `brew install git@2.50` (if the formula exists)
     or pin via `brew extract git 2.50.2` into a tap.
   - Source: build git 2.50.x from tag, install to `/usr/local/git-2.50/`.

3. **Clean up damaged refs** on repos that were hit before the fix:

   ```bash
   # Dry-run (default): see what would be pruned
   am doctor fix-orphan-refs --all --format json

   # Review, then apply
   am doctor fix-orphan-refs --all --apply
   ```

   Backups land under `<STORAGE_ROOT>/backups/refs/<project_slug>/`
   (last 10 kept) so you can restore manually if we over-pruned.

### Verify

After applying remediation:

```bash
# Run a synthetic stress loop that exercises the exact race.
# Requires the git-251-racer fixture (bead H1).
cargo test -p mcp-agent-mail-storage --test it libgit2_index_race_immunity:: \
  -- --nocapture
# Opt in via AM_C8_RUN=1; gated because it deliberately triggers
# CLI git segfaults on 2.51.0 boxes.

# Kernel log should show zero new segfaults during the next session:
journalctl -k --since '10 minutes ago' | awk '/segfault/ && /git\[/' | wc -l
# Expected: 0 when AM_GIT_BINARY is pointing at a safe git.
```

### Further reading

- `docs/GIT_251_FINDINGS.md` — full bug analysis with disassembly,
  vendor matrix, and reproduction procedure.
- `docs/DESIGN_git_lock.md` — how mcp-agent-mail serializes its own
  git calls to avoid racing against itself.
- `docs/GIT_SHELLOUT_AUDIT.md` — inventory of every in-process git
  shell-out in the workspace and its migration status.
