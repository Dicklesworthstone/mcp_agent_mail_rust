# MCP Agent Mail Operator Cookbook

Canonical copy-paste recipes for common operator workflows. Use this when you
need a known-good command sequence quickly; use
[`docs/OPERATOR_RUNBOOK.md`](OPERATOR_RUNBOOK.md) for deeper operational
background and TUI-specific troubleshooting.

> Replace placeholders such as `/abs/path/project`, `BlueLake`, `THREAD_ID`,
> `MESSAGE_ID`, and `age1...` before running anything.
>
> Recipes marked "read-only" inspect state. Recipes marked "stateful" write
> config, register agents, reserve files, send mail, or export bundles.
>
> Use targeted messaging only. `am mail send` requires explicit `--to`
> recipients; broadcast messaging is intentionally unsupported.

## 1. Bootstrap a local checkout and inspect setup [read-only]

**Goal:** See what `am setup` would configure before writing any local config.

```bash
cd /abs/path/project
am setup run --dry-run --yes --project-dir "$PWD" --format toon
am setup status --format toon
am setup status --format json
```

**Expected output:** A dry-run summary showing which config files would be
written or skipped, followed by a status report for the detected shell/editor
integration. Status output includes `primary_drift_reason`, `risk`, current and
expected server entries, and a remediation command. Tokens are redacted.

**Troubleshooting:** If the detected host, port, or path are wrong for this
checkout, rerun with `--host`, `--port`, or `--path`. For bearer-header drift,
provide the expected token with `--token` or through `HTTP_BEARER_TOKEN` /
`config.env`. When the dry run looks correct, apply the exact same target with
an explicit fix command:

```bash
am setup run --dry-run --project-dir "$PWD" --format toon
am setup run --yes --project-dir "$PWD" --format toon
```

## 2. Start a local HTTP server on a custom port [stateful]

**Goal:** Bring up a local MCP HTTP server quickly for manual testing.

Terminal A:

```bash
am serve-http --host 127.0.0.1 --port 9000 --no-auth --no-tui
```

Terminal B:

```bash
curl http://127.0.0.1:9000/health
```

**Expected output:** Terminal A stays attached to the server process. Terminal B
returns a health payload or status response proving the server bound to the
requested port.

CLI mail, inbox, agent, contact and reservation commands discover a custom
port when the same mailbox has one exclusive daemon owner and its recorded
listener hint still matches the operating system's listener PID. This needs
the same temporary directory and readable process/listener information.
Dead or ambiguous listeners are not selected. Explicit `AGENT_MAIL_URL`,
`HTTP_HOST` or `HTTP_PORT` settings take precedence over discovery. If that
explicit endpoint fails, the ownership refusal names the verified listener
and an `AGENT_MAIL_URL` override you can use to reach it.

To select the endpoint explicitly across repositories, export
`AGENT_MAIL_URL=http://127.0.0.1:9000/mcp/` in your shell, or set `HTTP_PORT`
in the user configuration file (`$XDG_CONFIG_HOME/mcp-agent-mail/config.env`).
`am config set-port` updates the working-directory `.env`; it does not export
a variable into other shells. Process variables override user configuration,
which overrides the working-directory `.env`.

**Troubleshooting:** If the port is already in use, choose another `--port`
instead of killing the existing server. If you expect auth to be enabled, drop
`--no-auth` and make sure `HTTP_BEARER_TOKEN` resolves from your env file.

For a slow boot-time integrity probe, `INTEGRITY_CHECK_ON_STARTUP=false` skips
only that startup probe. The background guard remains enabled and starts its
first cycle after `INTEGRITY_QUICK_CHECK_INTERVAL_SECONDS` (default 300 seconds,
minimum 1). The integrity verdict stays yellow with the configured skip and
scheduled time until a full check passes; a failed check stays red. The full
check cadence remains `INTEGRITY_CHECK_INTERVAL_HOURS` (default 1; `0` disables
scheduled full checks).

Keep `INTEGRITY_GUARD_ENABLED=true` to retain periodic checks and automatic
backups. Setting it to `false` disables the entire worker, including database
maintenance. To disable only checkpoint/ANALYZE/VACUUM/journal-size maintenance,
use `DB_MAINTENANCE_ENABLED=false`; checks and backups still run. These settings
take effect when the server starts. Existing service overrides remain an
operator decision; inspect them before changing a deployment.

### Verify a mailbox before upgrading [read-only]

Use the installed binary and the same `STORAGE_ROOT` and `DATABASE_URL` as
the service. Stop the service gracefully through its supervisor and prevent
automatic restarts while inspecting or copying the mailbox. Then run:

```bash
am doctor locks --json
am doctor drain --json
am doctor check --json
am doctor archive-verify --json
am doctor backups --json
```

Inspect `safe_to_mutate` in the drain report; its exit code alone does not
prove the owner has stopped. Resolve findings before upgrading. Preserve an
offline copy of the entire storage directory and, if the database lives
elsewhere, its complete database family, including WAL, SHM, engine sidecars,
and recovery artifacts. Keep the old binary and the diagnostic output. Copy
only while all mailbox writers remain stopped; copying the main SQLite file
alone can omit committed WAL data.

There is no required standalone forced-checkpoint command in this procedure.
Do not delete WAL, SHM, certificate, or namespace files to make an upgrade
start. `am doctor check` is verification, not a promise to checkpoint or
migrate. Start the new binary with the same configuration, then repeat the
checks and review startup logs. Preserve the offline copy until verification
passes. A support bundle can retain sanitized diagnostic evidence:

```bash
am doctor support-bundle --output-dir /abs/path/upgrade-evidence --json
```

For GH#357, startup may reconcile archive-ahead state before normal pool
migrations. Bounded replay now prepares its required additive message columns
transactionally through canonical SQLite before querying them; the ordinary
migration runner subsequently records the migrations and performs dependent
work. When columns are added, canonical record materialization preserves
legacy row values before runtime reads resume. Replay rechecks missing IDs in
its write transaction and leaves already present messages untouched. If
post-apply inventories still disagree, startup stops without reconstructing.
A replay/schema error rolls back and stops reconciliation without
reconstructing a healthy primary. Existing corruption recovery still has its
separate health checks and recovery admission controls.

Archive-ahead means inventory drift, not confirmed corruption. A successful
reconciliation message does not establish damage. The reported 984-versus-982
upgrade discrepancy has not been reproduced here; stale WAL certification is
an unverified hypothesis. Keep the complete pre-upgrade database family and
logs if inventories disagree. Do not infer missing committed rows from an
`immutable=1` read of a main file when a populated WAL exists: that mode ignores
the WAL.

## 3. Register a named operator agent [stateful]

**Goal:** Create or refresh an explicit agent identity for a project.

```bash
PROJECT=/abs/path/project
AGENT=BlueLake

am agents register \
  --project "$PROJECT" \
  --program codex-cli \
  --model gpt-5 \
  --name "$AGENT" \
  --task "Operator sweep" \
  --format toon

am agents show --project "$PROJECT" "$AGENT" --format toon
```

**Expected output:** The register call returns the agent profile that was
created or refreshed. The follow-up show command prints the current stored
metadata for that exact agent name.

**Troubleshooting:** If the name is rejected, pick a valid adjective+noun name
or omit `--name` and let the CLI generate one. If the project key is wrong, the
agent will be registered in the wrong archive namespace.

## 4. Triage urgent inbox items and ack backlog [read-only]

**Goal:** See what needs attention first for one operator.

```bash
PROJECT=/abs/path/project
AGENT=BlueLake

am robot status --project "$PROJECT" --agent "$AGENT" --format toon
am robot inbox --project "$PROJECT" --agent "$AGENT" --urgent --format toon
am robot inbox --project "$PROJECT" --agent "$AGENT" --ack-overdue --format toon
```

**Expected output:** A compact status summary, then the urgent inbox, then a
focused list of ack-overdue items. (`--format md` is only accepted by
`am robot thread` and `am robot message`.)

**Troubleshooting:** If the inbox comes back empty unexpectedly, confirm the
agent name spelling with `am agents list --project "$PROJECT"`. If the mailbox
is busy, wait for the current long-running operation to finish and retry.

## 5. Inspect a bead thread and a specific message [read-only]

**Goal:** Jump from a bead ID to the matching thread and then to one message.

```bash
PROJECT=/abs/path/project
AGENT=BlueLake
THREAD_ID=br-o217s.12
MESSAGE_ID=14

am robot search "$THREAD_ID" --project "$PROJECT" --agent "$AGENT" --format toon
am robot thread "$THREAD_ID" --project "$PROJECT" --agent "$AGENT" --format md
am robot message "$MESSAGE_ID" --project "$PROJECT" --agent "$AGENT" --format md
```

**Expected output:** Search results that confirm the thread exists, a rendered
thread transcript, and then a deep view of the selected message.

**Troubleshooting:** If `MESSAGE_ID` is wrong, rerun the thread view first and
pick a valid message ID from the recent messages. If the bead has no thread yet,
search by subject text instead of the bead ID.

## 6. Review one agent's last 24 hours [read-only]

**Goal:** Pull a compact recent activity review without guessing timestamp
syntax.

```bash
PROJECT=/abs/path/project
AGENT=BlueLake
SINCE="$(python3 - <<'PY'
from datetime import datetime, timedelta, timezone
print((datetime.now(timezone.utc) - timedelta(hours=24)).isoformat(timespec='seconds').replace('+00:00', 'Z'))
PY
)"

am robot timeline --project "$PROJECT" --source "$AGENT" --since "$SINCE" --format toon
am robot analytics --project "$PROJECT" --agent "$AGENT" --format toon
```

**Expected output:** A timeline covering the last 24 hours and an analytics
summary highlighting anomalies or remediation hints, if any.

**Troubleshooting:** `--since` expects an ISO-8601 timestamp, not shorthand such
as `24h`. If the timeline is too noisy, add `--kind` or `--source` filters.

## 7. See what work is ready before assigning agents [read-only]

**Goal:** Check the bead queue without leaving the CLI.

```bash
cd /abs/path/project
am beads status --format toon
am beads ready --limit 10 --format toon
am beads show br-o217s.12 --format toon
```

**Expected output:** A project status summary, then the current ready queue, and
then a detailed view of one candidate bead.

**Troubleshooting:** If the bead is not ready, inspect its blockers in the show
output before assigning it. For graph-level prioritization, use `bv
--robot-triage` instead of the interactive `bv` TUI.

## 8. Check reservation contention before editing [read-only]

**Goal:** See who already holds the files you want to touch.

```bash
PROJECT=/abs/path/project

am robot reservations --project "$PROJECT" --all --conflicts --format toon
am file_reservations conflicts "$PROJECT" README.md AGENTS.md
am file_reservations active "$PROJECT" --limit 20
```

**Expected output:** A robot-formatted conflict summary, a direct conflict check
for the named paths, and a broader list of active reservations in the project.

**Troubleshooting:** If you see a conflict, wait for the TTL to expire or
coordinate in-thread with the holder before editing. Do not work around the
reservation by touching the file anyway.

## 9. Release a crashed agent's reservations [stateful]

**Goal:** Clean up stale reservations after confirming an agent is no longer
active.

```bash
PROJECT=/abs/path/project
AGENT=BlueLake

am agents show --project "$PROJECT" "$AGENT" --format toon
am file_reservations active "$PROJECT"
am file_reservations release "$PROJECT" "$AGENT"
```

**Expected output:** The agent profile confirms who owns the reservations, the
active list shows what is currently leased, and the release command drops that
agent's active reservations.

**Troubleshooting:** Make sure the agent is actually idle before releasing its
leases. If you only want a subset, use `am file_reservations release "$PROJECT"
"$AGENT" --paths <path>`.

## 10. Send a targeted urgent message that requires acknowledgement [stateful]

**Goal:** Notify one agent about a blocking condition without spamming everyone
else.

```bash
PROJECT=/abs/path/project
FROM=BrownDove
TO=BlueLake
THREAD_ID=br-o217s.12

am mail send \
  --project "$PROJECT" \
  --from "$FROM" \
  --to "$TO" \
  --subject "[${THREAD_ID}] Action required" \
  --body "Please stop new edits and reply in-thread." \
  --thread-id "$THREAD_ID" \
  --topic "release.blocker" \
  --importance urgent \
  --ack-required \
  --format toon
```

**Expected output:** A delivery summary showing the message was queued for the
explicit recipient and attached to the requested thread.

`--topic` is optional. Use it when several threads belong to one stable work
stream: topics are case-insensitive, 1-64 safe ASCII characters, and replies
inherit the original topic. Use `--thread-id` for conversation identity and
`--topic` for cross-thread grouping; they are deliberately separate fields.

**Sender token (proving identity):** `am mail send` accepts a per-agent
`sender_token`. If `--from` was registered with `am agents register` or
`am macros start-session`, the token is persisted locally and reused
automatically — no extra flags needed. To supply it explicitly without echoing
the raw secret into shell history, prefer one of:

```bash
# From a file (contents trimmed):
am mail send ... --sender-token-file ~/.config/mcp-agent-mail/sender.token

# From the environment (not visible in `ps`/history):
AGENT_MAIL_SENDER_TOKEN="$(cat ~/.config/mcp-agent-mail/sender.token)" \
  am mail send ...
```

Resolution precedence: `--sender-token` (discouraged — visible in the process
list) > `--sender-token-file` > `AGENT_MAIL_SENDER_TOKEN` > persisted identity.

**Troubleshooting:** If the recipient is unknown, confirm the exact agent name
with `am agents list --project "$PROJECT"`. Do not look for a broadcast flag;
targeted delivery is the only supported path.

## 11. Export a mailbox bundle for a collaborator [stateful]

**Goal:** Produce a share bundle, preview it first, then export an encrypted
archive.

```bash
PROJECT=/abs/path/project
OUT=~/mailbox-share
AGE_RECIPIENT=age1example...

am share export --output "$OUT" --project "$PROJECT" --dry-run
am share export --output "$OUT" --project "$PROJECT" --zip --age-recipient "$AGE_RECIPIENT"
am share verify "$OUT"
```

**Expected output:** The dry run summarizes what would be exported. The real
export writes the bundle directory to `OUT` and the encrypted archive beside it
(`$OUT.zip.age`; `$OUT.zip` without `--age-recipient`). `am share verify` checks
a bundle directory, not an archive: to check what you will ship, run
`am share decrypt "$OUT.zip.age"` (it writes `$OUT.zip`), unzip that, and verify
the unpacked directory. It hashes every file the export wrote against the
manifest (`Files valid`), so with a signature the whole bundle is covered; the
hosting config the deploy wizard rewrites per provider (`_headers`,
`_redirects`, `.nojekyll`) is left out.

**Troubleshooting:** If you need a different scrub profile or chunking behavior,
add the relevant `am share export` flags explicitly. Use a real Age recipient
before dropping `--dry-run`.

## 12. Dry-run a legacy Python import or upgrade [stateful]

**Goal:** Inspect legacy state before performing any migration.

```bash
am legacy detect
am legacy status --format toon
am legacy import --auto --search-root "$HOME" --dry-run --format toon
am upgrade --search-root "$HOME" --dry-run --format toon
```

**Expected output:** Detection/status output describing legacy installations,
followed by dry-run plans for import and upgrade actions.

**Troubleshooting:** Narrow `--search-root` if the scan is too broad or too
slow. Keep the first pass as a dry run so you can review the detected paths
before changing anything.

## 13. Run doctor checks and inspect backups [read-only]

**Goal:** Collect health diagnostics before attempting manual repairs.

```bash
PROJECT=/abs/path/project

am doctor check "$PROJECT" --format toon
am doctor backups --format toon

# Reliability diagnostics (all read-only; run before any mutating repair):
am doctor health                      # one-line multi-verdict health rollup; exit 1 on findings
am doctor locks --json                # owner intelligence: who holds the mailbox (live/wedged/stale)
am doctor drain                       # is it safe_to_mutate right now? (no live owner)
am doctor mcp-selftest --format json  # live MCP round-trip self-test
am doctor reclaim --dry-run           # preview consolidation of stale .doctor run debris
am doctor support-bundle --json       # sanitized incident bundle for maintainer triage (no raw DB/bodies)
```

**Expected output:** `doctor check` reports archive or database problems, and
`doctor backups` lists any available backup snapshots or recovery artifacts.
`doctor locks` / `doctor drain` tell you whether a live `am` still owns the
mailbox — `repair` and `reconstruct` refuse while a live owner is present, so
drain it via your supervisor first (never kill `am` directly).
`doctor health` prints `warn: reservation parity: reconciling N ...` and exits
0 when the only reservation drift is releases the server's background
reconciler is visibly republishing (one repaired within the last 10 minutes).
If that reconciler is not running or has stopped making progress, health exits
1 and names the `am doctor fix --only ... --yes` command that repairs the drift.

**Troubleshooting:** If the mailbox lock is busy, wait for the current archive
operation to finish and retry. Run repair commands only after reading the doctor
output; diagnosis should come before mutation.

### Automatic backup retention and actual disk reclamation

Successful automatic publication runs retention immediately, including during
long-running server operation. On supported Unix systems the default policy
physically removes superseded periodic backup generations when verification
and filesystem checks establish that doing so preserves recovery. Rotation of
incident evidence into `doctor/reclaimable/` remains a move: those bytes still
occupy the filesystem.

`AM_BACKUP_KEEP_COUNT` requests the newest three generations per backup kind by
default, with a minimum of one. The current `.bak` publication slot and the
exact verified recovery generation remain protected, even when an older verified
generation falls outside that window. Automatic deletion accepts only strict
timestamped periodic names, requires a fully verified newer restore point and
a valid standalone candidate, and proves common mailbox scope through a
matching nonempty database lineage or an exact byte-for-byte match to the
verified restore point. These checks apply to resident and historical files.
Companion recovery state, changed files, unsafe paths, or observed active
readers cause refusal. On Linux this includes memory mappings whose file
descriptors have already closed. A bad backup cannot become disposable merely
because its filename looks old.

Retention inventories both resident backups and material already accumulated
under `doctor/reclaimable/`. Native historical rotation files are eligible only
inside generated `rotation-YYYYMMDDTHHMMSSZ[-N]` directories, with the same
verification and scope proof as resident candidates. A recreated mailbox's
older lineage remains protected unless it is an exact duplicate. Recorded recovery
runs, including the offline script's `rotated-backups-*` and
`proactive-staging-*` directories, retain their complete manifest, action logs,
and payloads for inspection, resume, or restore. Successful move receipts do
not authorize deletion. Unrecognized material and failed or interrupted backup
stages remain recovery evidence.

Automatic exporters also have admission limits so refused reclamation cannot
silently produce unlimited new copies. The periodic count limit is
`keep_count + 3`, including retained historical periodic files. The projected
logical byte limit is the larger of 512 MiB and
`(keep_count + 3) × (current database bytes + WAL bytes)`. Existing periodic
images and the full observed `doctor/reclaimable/` footprint count toward that
byte limit; a proposed publication also reserves one current database-plus-WAL
estimate. Separate staging admission and disk-headroom checks still apply.
Metadata inspection now uses the same bounded staging namespace as backup
exports. Legacy `.snapshot-inspect-*.sqlite3` files left beside the database
by older interrupted inspections also spend the parent-wide staging count
and byte budget. They remain uncertain recovery evidence; automatic retention
does not delete them. A failed inspection preserves its new staging directory
and reports that path, while successful metadata publication removes it.
The resident retention window can shorten to restore count or byte capacity,
while preserving the publication slot and verified recovery generation. Only
confirmed removals reduce that calculation; a refused candidate cannot prevent
cleanup from considering another safely superseded generation in the window.
These are bounds on cooperating automatic producers, not a filesystem quota
on operator exports or other programs.

An old oversized backlog with missing or stale verification metadata may need
a fresh verified publication before any safe deletion can proceed. The
per-mailbox automatic-backup journal durably reserves **one** such repair export
after live verification and exclusive snapshot admission, immediately before
the first backup mutation. An ordinary export also passes its disk-headroom
check first; preserving stale companion recovery files can spend the allowance
before that later check. A reader or a failed live check can therefore defer
without spending the allowance. Once export begins, a crash, failed backup
validation, or failed metadata publication does not replenish that allowance.
Further over-budget exports remain refused
until an ordinary admission check observes sufficient retained count and byte
capacity after cleanup. Existing failed-stage admission is checked first. A
refused export still permits independently admitted, bounded reclamation of
superseded backups, including when interrupted staging or an unavailable
journal prevents publication, or when the producer refuses disk headroom or
fails verification before publishing. This leaves the preserved operation evidence
and export retry state intact. Do not remove the journal or authority lock to
bypass a refusal: inspect the preserved evidence and resolve the underlying
retention, permission, or verification finding.

Setting `AM_BACKUP_ROTATION_DELETE=false` explicitly preserves move-only
retention and disables the extra repair export; automatic publication stops at
the same admission limits when staging cannot release space. Leaving the
variable unset enables the verified periodic reclamation policy. Setting it
to `true` also enables the older incident-artifact deletion behavior, so it is
broader than the default periodic policy.

Startup and publication logs report `bytes_staged`, `bytes_deleted`,
`bytes_reclaimed`, and `bytes_retained`. Staged and retained values are logical
file lengths still occupying storage. Deleted bytes are the logical lengths
of confirmed removals; reclaimed bytes count the allocated blocks associated
with verified periodic unlinks. The latter is not a measurement of the
filesystem's free-space delta: filesystem snapshots and shared storage can
retain blocks independently. Refused operations retain their byte accounting.
An incomplete inventory refuses mutations and marks totals as lower bounds;
uncertain moves or unlinks have separate counters rather than being reported
as successful reclamation. Each cleanup pass attempts at most 128 periodic
candidates, with a candidate-byte budget of the larger of 512 MiB and the
current database's size. One oversized candidate can still be attempted so a
large mailbox can make progress. Verifying the pinned restore point adds I/O
beyond that candidate budget. A shared progress cursor keeps refused older
files from repeatedly preventing examination of the remaining backlog.
Bounded cleanup passes can leave more work for a later attempt; the publication
limit remains in force while that work is pending.

## 14. Plan archive pack maintenance [read-only]

**Goal:** Measure mailbox archive bloat without changing the Git archive.

```bash
am doctor pack-archive --plan
am doctor pack-archive --plan --json
git -C "$STORAGE_ROOT" count-objects -vH
```

**Expected output:** The planner reports global archive size, per-project
archive sizes, loose-object and packfile counts, pack ages, top artifact
categories, threshold verdicts, and exact safe follow-up commands.

**Safety:** The planner is read-only. It never runs `git maintenance`, never
prunes objects, and never deletes user data. If the verdict recommends action,
preserve the JSON output as evidence before running the stateful command:

```bash
am doctor pack-archive --json
```

`pack-archive` uses Git's loose-object and incremental-repack maintenance. That
can rewrite packfiles and temporarily increase disk usage while Git builds new
packs, but it does not prune live archive data. Do not run lower-level cleanup
commands unless a human has separately approved the exact command and risk.

## 15. Capture a quick benchmark baseline [read-only]

**Goal:** Get a fast performance snapshot before or after a change.

```bash
cd /abs/path/project
am bench --list
am bench --quick
am bench --quick --save-baseline /tmp/am-bench-baseline.json
```

**Expected output:** The available benchmark set, a quick benchmark run, and a
saved baseline file you can compare later.

**Troubleshooting:** If you only care about a subset, rerun with `--filter`.
For release-signoff performance work, capture the baseline on a machine that is
not already saturated by other agent builds.
