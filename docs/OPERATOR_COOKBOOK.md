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

**Troubleshooting:** If the port is already in use, choose another `--port`
instead of killing the existing server. If you expect auth to be enabled, drop
`--no-auth` and make sure `HTTP_BEARER_TOKEN` resolves from your env file.

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
am robot inbox --project "$PROJECT" --agent "$AGENT" --urgent --format md
am robot inbox --project "$PROJECT" --agent "$AGENT" --ack-overdue --format toon
```

**Expected output:** A compact status summary, then a human-readable urgent
inbox view, then a focused list of ack-overdue items.

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

am robot timeline --project "$PROJECT" --agent "$AGENT" --since "$SINCE" --format md
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
  --importance urgent \
  --ack-required \
  --format toon
```

**Expected output:** A delivery summary showing the message was queued for the
explicit recipient and attached to the requested thread.

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
OUT=~/mailbox-share.zip
AGE_RECIPIENT=age1example...

am share export --output "$OUT" --project "$PROJECT" --dry-run
am share export --output "$OUT" --project "$PROJECT" --zip --age-recipient "$AGE_RECIPIENT"
am share verify "$OUT"
```

**Expected output:** The dry run summarizes what would be exported, the real
export writes the bundle to `OUT`, and verify confirms the resulting archive is
well-formed.

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
```

**Expected output:** `doctor check` reports archive or database problems, and
`doctor backups` lists any available backup snapshots or recovery artifacts.

**Troubleshooting:** If the mailbox lock is busy, wait for the current archive
operation to finish and retry. Run repair commands only after reading the doctor
output; diagnosis should come before mutation.

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

## 16. Audit canonical project-key split before swarm work [read-only]

**Goal:** Confirm all agents for one Git repository are using the same project
key before assigning or reserving work. This catches the common split where new
sessions use a remote-derived key while older panes still read path-shaped
projects.

```bash
cd /abs/path/project

CANONICAL_PROJECT="$(agent-mail-project-key)"
LEGACY_PROJECTS='[
  "/abs/path/project",
  "/older/checkout/path/project"
]'

printf 'canonical_project=%s\n' "$CANONICAL_PROJECT"
am robot projects --format json > /tmp/am-projects.json

jq --arg canonical "$CANONICAL_PROJECT" --argjson legacy "$LEGACY_PROJECTS" '
  .projects
  | map(select(.path == $canonical or (.path as $p | $legacy | index($p))))
  | map({
      slug,
      path,
      agents,
      messages,
      reservations,
      updated_at
    })
' /tmp/am-projects.json

for project in "$CANONICAL_PROJECT" $(jq -r '.[]' <<<"$LEGACY_PROJECTS"); do
  printf '\n# active reservations for %s\n' "$project"
  am robot reservations --project "$project" --all --format json \
    | jq '{project: ._meta.project, active: (.all_active // [])}'
done

am doctor drain --json | jq '{owner_class, safe_to_mutate, detail, safe_next_command}'
am doctor health

# Candidate/new-release inventory surface for reservation parity drift.
# This is read-only after the release that classifies `doctor fix --list` as
# read intent; do not substitute `--dry-run` or `--yes` while a live owner holds
# the mailbox.
am doctor fix --only fm-db-state-files-reservation-db-archive-parity --list --json \
  | jq '{mode, fm_id, findings_count, actions_planned}'
```

`--list` is detector inventory only: it does not create a doctor run, execute the
chokepoint, or mutate mailbox state. Some FM outputs may still show
`actions_planned` / `total_actions_planned` for auto-fixable findings; treat
that as "actions that would exist under an approved `--dry-run`/`--yes` path",
not as permission or evidence that this read-only audit changed anything.

**Expected output:** The canonical project is the only project with active file
reservations. Legacy/path-shaped projects may still contain historical messages
or agents, but active reservations there mean some panes are operating on the
wrong bus and must be coordinated before new edits start.

**Safety:** This recipe is read-only. Do not release, force-release, archive
normalize, reconstruct, search-index refresh, restart the server, or edit the
database from this audit. If `am doctor drain` reports `safe_to_mutate=false`,
that is normal for a live service owner; use it as a no-mutation guard, not as a
reason to kill the owner. On deployed versions before the read-intent
classification fix, `am doctor fix --only <fm-id> --list` may still be blocked
by the live-owner guard; use a staged/candidate binary for that inventory probe
until the live version is upgraded.

**No-go criteria:**

- The `agent-mail-project-key` helper is missing or returns an empty value.
- Both the canonical project and any legacy project have active reservations.
- `am doctor health` reports archive/DB parity drift or DB sanity failures that
  would make project/reservation reads ambiguous. A stale search index is a
  search-only hold; do not refresh search from this audit unless the operator
  explicitly needs message-search evidence.
- The installed `am` version is not distinguishable from the candidate release
  intended to fix the coordination issue.

**Next safe action:** install or update the local `agent-mail-project-key`
helper on the affected host, point new agents at the canonical project key, and
wait for old legacy reservations to expire or be explicitly coordinated by their
owners. Only run stateful doctor fixes after the live service owner has been
gracefully drained and an operator has approved the exact mutation.

## 17. Stage hardening uptake without touching the live hub [local-artifact-only]

**Goal:** Prove a candidate binary and runbook path before any service restart
or live install.

```bash
cd /abs/path/mcp_agent_mail_rust
scripts/hardening_uptake_no_live_hub.sh --profile release
sed -n '1,120p' target/hardening-uptake/*/artifacts/uptake_report.txt
```

**Expected output:** The script writes a passed or failed `uptake_report.txt`
containing the source commit, staged binary path, live binary path, Cargo target
directory, disk preflight fields, WAL test results, installed-binary parity
command, `version_gate`, and `install_gate`. It never replaces the live binary,
never runs any stateful doctor fixer, and never starts or stops the Agent Mail
service. The default disk preflight minimum is 64 GiB because a cold test build
can exceed smaller tmpfs mounts before a report can be written. If it reports
`failed_step=disk_preflight`, move the proof to a host/workspace with enough
free space or get operator approval for cleanup; do not discover disk pressure
by forcing the linker to continue. On a disk-full host with enough tmpfs space,
use an off-repo stage root and Cargo target directory:

```bash
CARGO_TARGET_DIR=/dev/shm/am-hardening-cargo-target \
  scripts/hardening_uptake_no_live_hub.sh \
  --profile release \
  --stage-root /dev/shm/am-hardening-uptake
```

**Rollback:** There is no live rollback for this recipe because it only writes
under the chosen stage root and Cargo target directory. Close the canary shell
or remove the temporary `PATH=<stage-bin>:"$PATH"` prefix. A later live install
must have its own operator-approved rollback plan, usually reinstalling the
previous released binary and restarting through the service supervisor.
