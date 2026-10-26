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
post-apply inventories still disagree, the reconcile fails without
reconstructing; a primary that passed its health check keeps serving, with the
drift left pending for the periodic retry. The startup warning says to run
`am doctor health`, then `am doctor reconstruct --yes` to retry under
supervision.
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

### Read reservations through an already running server

A healthy server can keep serving its admitted database connections while a
new CLI read-only connection is refused with `database is busy (recovery in
progress)` (GH#368). Select the existing server's authoritative MCP resource
with `--server`; this read mode never opens, migrates, repairs, or reconstructs
the local mailbox and does not require restarting the owner.

```bash
PROJECT=/abs/path/project
export AGENT_MAIL_URL=http://127.0.0.1:8765/mcp/
# Supply the existing server credential through AGENT_MAIL_TOKEN or
# HTTP_BEARER_TOKEN when authentication is enabled; do not disable server auth.

am reservations --project "$PROJECT" --all --json --server
am file_reservations active "$PROJECT" --server
am robot navigate "resource://file_reservations?project=$PROJECT&active_only=true" \
  --project "$PROJECT" --json --server
```

`--server` is also supported by `am robot reservations` and by
`am file_reservations list`, `soon`, and `conflicts`. JSON fields, table columns,
agent filtering, expiry warnings, conflict detection, and navigation ordering
remain the same. Reservation attestation identifies `source: "mcp_server"`;
the database/archive comparison fields are null because no local database was
read. Without `--server`, the commands retain their direct-read behavior.

The endpoint follows the existing client configuration (`AGENT_MAIL_URL`, or
the configured `HTTP_HOST`, `HTTP_PORT`, and `HTTP_PATH` with existing local
owner discovery). `AGENT_MAIL_TOKEN` takes precedence over the existing
configured/service bearer token. Select the intended endpoint explicitly when
more than one mailbox is available. The CLI verifies the stored project ID,
slug, human key, creation timestamp, and agent roster before and after reading;
an absolute project path must match the stored path, including canonical path
aliases. Navigation rejects a URI project that disagrees with `--project` or
`AGENT_MAIL_PROJECT`. It does not infer a different project from a colliding
slug or create a missing project. An explicit `--agent` must be present in the
server's available-agent roster.

Authentication failure, an unavailable server, changed/mismatched identity,
redirects, malformed responses, and incomplete lists exit nonzero without
printing a reservation result or falling back to local/archive data. Automation
must check the exit status before using stdout or deciding that no leases exist.

**Completeness limit:** The existing reservation resource caps a response at
250 rows and has no multi-page snapshot token. Server mode accepts only a
short page (at most 249 matching reservations) and refuses a full 250-row page,
even for a smaller CLI `--limit` or navigation page. This prevents independent
OFFSET requests from skipping live leases during concurrent changes. Navigation
sorts the verified complete result into its existing newest-first order before
applying `limit`/`offset`. For larger projects, use a successful direct read for
a complete safety decision. The documented MCP resource's paged reads remain
available for manual inspection, but independently fetched pages are not an
atomic reservation snapshot.

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

Empty native rotation directories are also removed, including directories
emptied by the current reclamation pass. This releases directory inodes as well
as backup-file storage. Cleanup inspects only immediate, strictly named native
rotation directories and uses an empty-directory removal through retained
directory handles. Unknown directories, nested trees, manifests, and active
directory readers or staging operations remain protected. A bounded empty-only
sweep runs before the full backup inventory, so an old installation with more
than 16,384 empty rotation directories can drain over successive passes instead
of remaining permanently blocked by the inventory limit. Each rotation removes
at most 128 empty directories across its pre-inventory and post-reclamation
sweeps. Explicit `AM_BACKUP_ROTATION_DELETE=false` also disables this cleanup.

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
to `true` also allows incident-backup reclamation. On Unix, this requires an
older, standalone SQLite image with a fully verified successor, matching
mailbox lineage or exact duplicate bytes, and the same pin, reader, hardlink,
and namespace checks used for periodic generations. Corrupt or insufficiently
verified incident evidence stays available even with explicit deletion enabled.
Platforms without the verified descriptor-bound deletion boundary preserve
incident evidence. Explicit opt-in never falls back to unchecked pathname deletion.

Retention also holds the existing database recovery election from selection
through reclamation. A live recovery, an occupied legacy `.recovery.lock`, or
an unresolved recovery receipt ending in `.pending` prevents cleanup. The
verified survivor's inode, links, timestamps, and companion state are checked
again before each destructive operation. Explicit CLI restore holds the same
election before copying its backup, so retention cannot remove its source
during staging. Resolve pending recovery operations through recovery tooling;
removing their markers does not establish that their evidence is disposable.

Startup and publication logs report `bytes_staged`, `bytes_deleted`,
`bytes_reclaimed`, and `bytes_retained`; scheduled retention also reports
`empty_directories_deleted` separately from backup payload counts and bytes.
Staged and retained values are logical file lengths still occupying storage.
Deleted bytes are the logical lengths of confirmed removals; reclaimed bytes
count the allocated blocks associated with verified backup unlinks. The latter
is not a measurement of the
filesystem's free-space delta: filesystem snapshots and shared storage can
retain blocks independently. Refused operations retain their byte accounting.
An incomplete inventory refuses backup-file mutations and marks totals as lower
bounds; the separately admitted empty-directory sweep can still make progress.
Uncertain moves or unlinks have separate counters rather than being reported
as successful reclamation. Each cleanup pass attempts at most 128 periodic
candidates, with a candidate-byte budget of the larger of 512 MiB and the
current database's size. One oversized candidate can still be attempted so a
large mailbox can make progress. Verifying the pinned restore point adds I/O
beyond that candidate budget. A shared progress cursor keeps refused older
files from repeatedly preventing examination of the remaining backlog.
Explicit incident reclamation has a separate 128-candidate allowance and the
same candidate-byte limit, so incident verification cannot spend the periodic
queue's entire cleanup allowance.
Bounded cleanup passes can leave more work for a later attempt; the publication
limit remains in force while that work is pending.

`am doctor health` prints a `backup_retention:` line for this state. It reads
`storage.sqlite3.bak.meta.json` the way a verified backup would and computes
the producer's count and byte footprint with the same rule as admission. An
unversioned sidecar written by v0.3.13 through v0.3.30 is a warning: the next
verified backup keeps it as a `.legacy-<sha256>.json` sibling and replaces it.
Health fails (exit 1) when verified backups cannot replace the sidecar
(malformed, foreign, from a newer format, or not a regular file), because
verified backups then keep failing and rotation keeps every backup generation.
It also fails when the footprint is at the publication limit, because
automatic backups are then paused. For an unrecognized sidecar, move it aside
and keep it as evidence; the next verified backup records fresh metadata.
While automatic backups are paused, retention can delete superseded
generations only after a verified backup has recorded current metadata, and
the server makes at most one verified repair backup while over the limit. If
health still does not show `snapshot_metadata=current` after the next backup
attempt, stop the server and remove old `storage.sqlite3.bak.<timestamp>`
files by hand, keeping `storage.sqlite3.bak` and the newest few. Generations
that retention cannot tie to this mailbox also stay where they are; if
`backup_images` stops falling, remove them the same way.

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

## 16. Save and diagnose a private mailbox archive [read-only preview; stateful save]

**Goal:** Complete a restorable backup within an existing maintenance window,
and identify the expensive phase when it does not finish. These observations
address the visibility gap in
[GH#365](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/issues/365);
they do not establish the cause of the reporter's earlier slow saves.

**Build requirement:** Check the actual binary's command help. The installed
v0.3.39 from that report does not expose these progress and diagnostic options,
and a newer source build may still report package version `0.3.39`:

```bash
# From a checkout containing these changes, with the default target directory:
cargo build --locked --release -p mcp-agent-mail-cli --features portable --bin am
./target/release/am archive save --help
```

If `CARGO_TARGET_DIR` is customized, use the binary from that directory.
Confirm that help lists `--progress`, `--diagnostics`, `--compression-level`,
`--exclude-recovery`, and `--dry-run`. Help does not create an archive. Use that exact
binary for the examples below, keeping the existing command's project
selectors, working directory, mailbox configuration, and timeout.

Use the same `DATABASE_URL` and `STORAGE_ROOT` as the service. Archives are saved
under `archived_mailbox_states/` in the detected project root. Keep output and
captured logs outside `STORAGE_ROOT` when possible. If the output directory is
inside storage, save excludes that exact subtree to avoid including previous
backups or its own temporary ZIP. The output directory cannot be the storage
root itself. Relative paths and directory aliases are resolved for this check.
Stop writers gracefully through the existing supervisor and confirm ownership
with `am doctor locks --json` and `am doctor drain --json`; inspect
`safe_to_mutate`, not just the exit status.
Keep the supervisor's recovery/startup tail and the existing outage deadline.
Run these commands only against an isolated replica or in the already scheduled
backup window; diagnosing a previous timeout does not require another daytime
archive attempt.

```bash
am archive save --label nightly --scrub-preset archive --progress text

# For a scheduled job, retain structured phase records alongside its log:
am archive save --label nightly --progress json \
  2>/abs/path/backup-evidence/archive-save.stderr.log

# Independent coarse-phase heartbeats, without the cooperative progress stream:
am archive save --label nightly --progress off --diagnostics \
  2>/abs/path/backup-evidence/archive-save.stderr.log
```

Text progress is enabled by default and goes to stderr, including when the
command runs without a terminal. `--progress json` emits structured progress
records; other error or warning diagnostics may also appear on stderr.
`--progress off` suppresses progress. The final archive path and success
summary remain on stdout. `--diagnostics` is a separate, optional stream that
is off by default. It can be combined with any progress mode.

### Preview the selected storage [read-only]

Use the same mailbox configuration and working directory as the scheduled job:

```bash
am archive save --dry-run
am archive save --dry-run --exclude-recovery
```

The preview reads filesystem metadata without opening SQLite, acquiring
mailbox activity locks, creating a snapshot, compressing files, or writing an
archive. It reports regular-file counts and bytes, selected recovery-copy totals,
selected and excluded bytes, and the selected compression level. A running
service can change the files during the scan. These totals are not a
compressed-size forecast, an integrity check, or a measurement of snapshot CPU
time. A large omitted-byte total does not establish which phase caused an
earlier timeout.

The preview does not validate attachment references against a database
snapshot; an actual save performs that check before applying the optional
recovery-copy exclusion. Run the preview before the maintenance window while
retaining the supervised stop/save/restart procedure and bounded timeout for
the real backup.

### Interpreting the phases

| Progress phase | Diagnostic phase | Work being measured |
|----------------|------------------|---------------------|
| `prepare`, `mailbox_lock` | `prepare` | Validate arguments and paths, then acquire mailbox activity locks before reading the source |
| `source_reconciliation` | `snapshot_source` | Compare the live database with the canonical archive and, if needed, reconstruct a private source; this may traverse archives and execute SQL before the main snapshot copy |
| `snapshot_prepare` | `snapshot` | Validate paths and prepare private staging for an unscoped lossless image |
| `snapshot_source_open` | `snapshot` | Open the resolved source through its guarded read-only engine |
| `snapshot_full_copy` | `snapshot` | Copy the full primary database, including committed WAL state, into a fresh private SQLite image |
| `snapshot_neutralize` | `snapshot` | Settle only the private image and retire its engine handoff sidecars |
| `snapshot_validate` | `snapshot` | Check the private image and gather mailbox counts |
| `snapshot_publish` | `snapshot` | Publish the completed private SQLite image to the save's staging location |
| `snapshot_copy` | `snapshot` | Read one consistent engine-owned transaction into a selected projection for a scoped or scrubbed export |
| `snapshot_scope` | `filter` | Apply project selection to a projected snapshot when requested |
| `snapshot_scrub` | `scrub` | Apply the selected scrub policy; the default `archive` preset preserves private recovery state |
| `snapshot_compact` | `finalize` | Produce the standalone projected snapshot; private backups build no additional viewer FTS indexes or materialized tables |
| `recovery_exclusion_check` | `archive_metadata` | Check the optional recovery-copy policy against the primary database path and both attachment path fields |
| `metadata` | `archive_metadata` | Read snapshot metadata and prepare archive metadata |
| `storage_inventory` | `storage_enumeration` | Enumerate selected regular storage files and their sizes, including recovery history |
| `compress_snapshot`, `compress_storage`, `finalize_zip` | `zip_writing` | Write metadata, snapshot, and storage payloads, then finish the ZIP central directory |
| `verify_zip` | `archive_verification` | Read back the completed ZIP and verify its members before publication |
| `publish` | `publication` | Sync the completed, verified ZIP, publish without replacing an existing destination, and sync parent directories on Unix |

Storage inventory precedes ZIP writing. The diagnostic `zip_writing` phase is
one continuous occurrence across the three finer compression/finalization
phases, followed by `archive_verification` and then `publication`.
The unscoped lossless image uses `snapshot_prepare` through
`snapshot_publish`, all under the coarse `snapshot` diagnostic. Project filters
or other scrub presets use the projected copy/scope/scrub/compact path.

Elapsed phase time measures this invocation. It is not an estimate of the
remaining maintenance window. A single SQL operation can take time between
progress callbacks; the last phase identifies where to investigate, while
process CPU and I/O samples help determine whether work is continuing.
Progress records carry `schema_version: 1`, the phase, a state such as
`started`, `progress`, `completed`, `cancelled`, or `failed`, and elapsed times.
File/byte counters reset for each phase; projected snapshot `rows_copied` counts
rows in the named table. A native full copy can spend time inside one database
statement without per-table row counts. `recovery_files` and `recovery_bytes`
count selected regular files beneath only `doctor/forensics/` and
`doctor/reclaimable/`; they do not classify the whole `doctor/` tree. Omitted
files contribute to `excluded_files` and `excluded_bytes` in metadata and the
dry-run report. Retain completed records to compare totals across phases.
Verification reads payloads again;
its phase-local byte count is verification work, not additional archive input.
The `snapshot_bytes` and `archive_bytes` fields are size gauges retained across
phase changes.

Cooperative progress is rate-limited to a two-second interval between
checkpoints, with immediate boundary records. It is not an independent timer
that proves a blocked operation is responsive. Cancellation is checked between
SQL pages and filesystem copy chunks; an individual database statement or
blocking filesystem call may still delay shutdown, so keep the supervisor's
hard limit. On Unix, the first SIGINT/SIGTERM requests cleanup at a checkpoint;
a second termination signal uses its default termination behavior and may leave
unpublished temporary files.

### Optional independent diagnostics and privacy

`--diagnostics` writes records prefixed `archive_save_diagnostic` to stderr.
Coarse phase start, completion, and failure records are immediate. While a
phase runs, an independent reporter emits at most one periodic `running`
record per five seconds on its cadence; a periodic record may be close to a
phase boundary. `heartbeat=1` means that worker started. `heartbeat=0` means
it could not start and only boundary records are available. Elapsed times use
a monotonic clock, and numeric values saturate instead of wrapping.

Diagnostic work counters accumulate across the whole invocation:

| Counter | Meaning |
|---------|---------|
| `storage_files` | Selected regular storage files discovered by inventory |
| `storage_bytes` | Sum of their observed sizes |
| `storage_size_unknown` | Unknown selected-file sizes; the current inventory requires metadata and fails rather than silently selecting an unknown size |
| `zip_files` | Entries whose input payload was fully accepted, including metadata and snapshot entries |
| `zip_bytes` | Uncompressed payload bytes accepted at progress checkpoints, including a partial current entry; verification reads do not increase this counter |
| `snapshot_bytes` | Most recently observed snapshot size, a gauge rather than a running sum |
| `archive_bytes` | Observed compressed size of the completed ZIP, also a gauge |

A zero size gauge may mean it has not yet been measured, and a snapshot can
shrink during finalization. Even after an entry's input is fully accepted,
compression or central-directory output may remain until ZIP writing completes.
These are observations rather than a completion percentage or an ETA.

SQLite work can continue while file and byte counters stay unchanged. A
heartbeat proves that the diagnostic reporter is running, not that the current
operation is advancing. Forced termination cannot reliably emit a failure
record. Phase timing alone does not prove a deadlock, establish a reconciliation
backlog, or attribute cost to recovery-directory size.

The progress records and prefixed diagnostic records contain fixed
phase/status/schema names and numeric times or counters. They do not include
paths, project selectors, labels, message bodies, credentials, SQL, or raw
errors. Normal command output and existing warnings/errors may still contain
operator details. To retain only the coarse diagnostics for review:

```bash
rg '^archive_save_diagnostic ' /abs/path/backup-evidence/archive-save.stderr.log \
  > /abs/path/backup-evidence/archive-save.diagnostics.log
```

Enabling diagnostics does not change payload selection, scrub policy,
compression, locking, or the maintenance deadline.

The snapshot source decision may reconstruct a private database if the live
index is missing or behind authoritative archive state. That is separate from
the daemon's periodic message, identity, and reservation reconcilers. Historical
`deferred reconcile` warnings count retries, often of the same entity; their
line count is not the number of outstanding repairs.

### Safe diagnostics for an existing run [read-only]

On Linux, sample the actual archive-save PID from the supervisor during its
already scheduled invocation. Do not launch another save just to collect these
observations:

```bash
SAVE_PID=12345  # replace with the verified archive-save process ID
date --iso-8601=seconds
cat "/proc/$SAVE_PID/io"
cat "/proc/$SAVE_PID/stat"
find "/proc/$SAVE_PID/fd" -mindepth 1 -maxdepth 1 -type l -printf '.' | wc -c
```

Record samples at a modest interval, such as the existing 60-second sampling
plan, together with phase output and the supervisor's CPU/wall-time summary.
Retain the PID's start time from `/proc/PID/stat` so PID reuse is not mistaken
for continuation of the same run. Permission errors or process exit mean the
sample is unavailable, not zero activity.

`rchar`/`wchar` report read/write activity including cached I/O;
`read_bytes`/`write_bytes` describe storage I/O and can remain flat when data is
cached. Growing CPU time with little storage I/O can be SQL computation or
compression. A fixed temporary database size or unchanged mtime does not prove
a deadlock or identify the active SQL statement. Descriptor counts show resource
growth but do not measure completion. Treat these as supporting observations;
the named phase and its counters provide the attribution. Keep file-descriptor
targets and mailbox paths private if collecting additional local detail.

Existing native diagnostics answer narrower questions:

| Command | What it can establish | What it does not establish |
|---------|-----------------------|----------------------------|
| `am doctor locks --json` / `am doctor drain --json` | Current ownership and whether the mailbox is drained | Which archive-save phase is consuming CPU |
| `am doctor check --json` | Database/namespace health and archive inventory findings | That an archive save will finish within its deadline |
| `am doctor archive-scan --json` | Archive hygiene findings | A current, deduplicated reconciler backlog or ZIP integrity |
| `am doctor archive-verify --json` | Read-only comparison of archive artifacts with SQLite | Verification of a saved mailbox ZIP; commit provenance is not checked by this pass |
| `am doctor artifacts --json` / `am doctor backups --json` | Recovery and backup inventory | Permission to discard an old-looking artifact |
| `am doctor reclaim --dry-run --json` | A preview of recovery-debris consolidation | A reduction in archived bytes: move-only consolidation still leaves those bytes under `doctor/reclaimable/` |

Run diagnostics at an appropriate existing inspection point; full archive
comparison itself traverses and reads data. The v0.3.39 binary in the report
has no archive-save phase-progress flag, and these health commands cannot
retroactively identify the expensive phase of an earlier invocation. A healthy
result is evidence against the specific failures checked, not a reason to run
an unrequested repair or reconstruct operation.

### Recovery history, compression, and verification

Compression settings range from `0` through `9`; the default is `6`.
`--compression-level 1` trades a larger ZIP for less compression work, level `0`
uses uncompressed ZIP storage, and levels `1` through `9` use deflate with
increasing compression effort. Compare
phase times and output sizes on isolated copies of the same fixture before
changing a production maintenance budget. A smaller archive is not necessarily
faster to produce, and host CPU contention affects wall time independently of
archive contents. For such a comparison, preserve the full recovery history:

```bash
am archive save --label compression-comparison --scrub-preset archive \
  --compression-level 1 --progress json
```

By default, all regular files under top-level `doctor/`, including forensic
bundles and `doctor/reclaimable/`, remain included. They may contain unique
recovery evidence or the bytes needed by a later undo. Their size alone does
not prove they caused a timeout, and age or a filename containing `reclaimable`
is not sufficient proof that they can be omitted. ZIP64 supports large
individual recovery files as well as archives with many members.

`--exclude-recovery` is an explicit opt-in policy; its visible alias is
`--exclude-recovery-artifacts`. Both omit only the exact
`STORAGE_ROOT/doctor/forensics/` and `STORAGE_ROOT/doctor/reclaimable/` trees
from the new ZIP. They do not prune, move, or delete source evidence or alter
existing backups. Retain the original generations or make a separate evidence
backup when they are still needed for investigation or rollback. Do not select
this policy when the backup must preserve complete recovery history.

The rest of `doctor/` remains included. In particular,
`doctor/backlog_dead_letter.jsonl` can contain serialized writes that have not
been applied to the archive, and `doctor/reservation-reconcile.json` records
reconciliation progress. `doctor/archive-quarantine/`, `doctor/quarantine/`,
recovery receipts, unknown files, `.archive_backlog/`, regular message and
attachment files, and Git history remain covered by the normal storage
traversal. A project's nested `doctor/` directory and similarly named paths
such as `doctor/forensics-extra/` are also retained. Do not replace the narrow
policy with a blanket `doctor/` exclusion.

Save refuses exclusion if the configured primary database or either attachment
path (`path` or `original_path`) in the selected snapshot points into an
omitted tree. The check handles normalized paths and filesystem aliases.
Unreadable attachment metadata also prevents exclusion because safety cannot
be established. Rerun without the flag to retain those files. These checks do
not establish that historical evidence is no longer needed for a separate
investigation or rollback.

Recovery bytes already stored in Git objects may still be included; exclusion
is not an erasure or redaction policy. Metadata records the compression level,
recovery-history policy, omitted storage paths, and selected/excluded byte
counts. These settings do not change the scrub preset or restore command.

The automatically excluded output subtree prevents recursive backup growth.
The dedicated SQLite snapshot remains included, while the embedded live
database and its sidecars are excluded from the storage payload. Storage
symlinks are not dereferenced. Snapshot staging lives beneath the excluded
output directory, and any private source staging within storage is explicitly
omitted. If no regular storage files are selected, an explicit `storage_repo/`
directory entry preserves the restore format and is checked by ZIP verification.

For a complete private mailbox backup, use the default `archive` scrub preset
without `--project`. This path preserves the resolved primary database's full
schema and data, including additional application tables, columns, indexes,
triggers, and identity state outside the share viewer's schema. Project filters
and other scrub presets intentionally produce selected or sanitized database
projections. Archive metadata records `database.source_kind`: if it is
`ArchiveSnapshot`, source resolution first reconstructed the mailbox from
storage, so the existing reconstruction limits still apply to that resolved
source. The full-image guarantee concerns the primary database; separate
runtime telemetry such as an external `atc.sqlite3` is not added automatically.

The current FrankenSQLite implementation materializes the database row image
in memory during `VACUUM INTO`; this is not a constant-memory snapshot algorithm.
Measure memory as well as elapsed time for a representative replica. ZIP
compression streams the separate storage files through bounded buffers. Source
authority changes during the native copy can still cause a safe refusal, so
the existing mailbox quiescence procedure remains necessary.

The `archive` preset is intended for a private backup and preserves sensitive
mailbox state. `--project` scopes the SQLite snapshot, while the selected storage
tree is included for restoration. A mailbox archive is therefore not a
sanitized share bundle; use the share/export workflow in section 11 for
publication. Published share bundles still build their viewer artifacts.

Before publication, save compares every ZIP member's name and uncompressed
size with the expected inventory, then reads each entry through EOF to check
its CRC, including the required snapshot and metadata members. This deliberate
verification pass consumes additional I/O and decompression work; its time is
reported separately. A failed or cancelled save does not replace an existing
good archive.

New ZIPs use owner-only permissions on Unix because private backups contain
registration credentials and mail. Publication uses the existing atomic
no-replace move: a competing backup, directory, or dangling symlink at the
chosen destination cannot be overwritten. The completed file is flushed before
the move, and parent directory updates are synced on Unix. Windows retains the
file flush and atomic no-replace move.

Before verification succeeds, a handled failure or cooperative cancellation
cleans up partial temporary ZIP output. Forced termination can still leave
unpublished temporary files. Source recovery evidence and existing good
archives are not removed by this cleanup.

If publication or its durability check fails, save reports failure and retains
the verified ZIP's staging directory for inspection. An already-published
destination is preserved. The error identifies both locations; inspect them
before retrying instead of assuming that no archive was created.

ZIP verification establishes container integrity. It does not prove that the
runtime database engine can restore and use the snapshot. Meaningful recovery
tests must also restore into an isolated mailbox and inspect database rows,
message/attachment files, and recovery evidence. `am archive restore --dry-run`
only prints the proposed destination and preservation operations. It does not
decompress every entry, verify the SQLite snapshot, or prove that restoration
will succeed. Preserve existing good archives until the new archive has passed
the required restore checks.

The completed component checks, paired compression measurements, and remaining
native test gates for issue #365 are recorded in
[Archive issue #365: validation record](ARCHIVE_365_VALIDATION.md).
