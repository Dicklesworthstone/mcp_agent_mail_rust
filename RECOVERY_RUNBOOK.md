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

## Keep pre-incident backups before restarting

A cold start of the HTTP/TUI server (no Agent Mail server already answering on
the port) rotates the backup files next to `storage.sqlite3`. It keeps the
newest `AM_BACKUP_KEEP_COUNT` (default 3, minimum 1) of each kind: `.bak*` and
`manual-backup-*`, `corrupt-*`, `reconstruct-*`, `archive-reconcile-*`,
`salvage-*`, and `pre-migrate*` / `pre-python-import*` / `pre-acfs-import*` /
`pre-reindex*`. Incident evidence is moved to
`<storage_root>/doctor/reclaimable/rotation-<timestamp>/`. Archive-reconcile
evidence remains owned by the separate recovery-retention planner.

Periodic dated `.bak.YYYYMMDD_HHMMSS[-NN]` generations have a different policy
on Unix: excess generations are deleted only when a newer hash-bound snapshot
passes full integrity verification under exclusive snapshot admission. The
exact `.bak` publication slot and the verified source are always retained.
Old `rotation-*` directories are swept with the same checks, one strict periodic
file at a time; incident files, unknown names, nested directories, symlinks and
hard-linked aliases are never automatically removed by this sweep.
Empty native rotation directories are removed separately in bounded passes,
including previously accumulated empty directories. Active staging leases,
unknown directories, nested trees and recorded recovery evidence are preserved.

Retention shares recovery's durable election. An active recovery, any occupied
legacy `.recovery.lock`, or an unresolved `.pending` recovery receipt prevents
reclamation. Explicit CLI restore acquires the election before staging its
source. These references are recovery evidence even when the original process
has exited; resolve the interrupted operation before retrying cleanup.

Unset `AM_BACKUP_ROTATION_DELETE` enables this periodic reclamation. Explicit
false preserves move-only retention. Explicit true also permits verified
incident deletion, requiring a fully verified successor, matching mailbox
lineage or duplicate bytes, and no live readers, aliases or companion state.
Unsupported platforms preserve incident evidence. Without safe reclamation,
automatic publication refuses at `max(1, AM_BACKUP_KEEP_COUNT) + 3` existing
periodic images, including prior rotation staging, or when retained logical
bytes plus the estimated new export exceed the larger of 512 MiB and
`(max(1, AM_BACKUP_KEEP_COUNT) + 3) × (current database bytes + WAL bytes)`.
The full observed reclaimable directory footprint spends that byte allowance.
A durable journal permits at most one extra repair export to restore missing
verification authority; a crash or failed export does not reset that allowance.
The periodic producer therefore stops adding copies when safe reclamation
cannot progress. Existing over-budget evidence is preserved. These admission
bounds apply to cooperating automatic exporters; they are not a filesystem
quota covering database growth, operator exports, or other programs. See the
[operator cookbook](docs/OPERATOR_COOKBOOK.md#13-run-doctor-checks-and-inspect-backups-read-only)
for the complete retention and repair policy.
When admission warns that its retention budget is exhausted, repair snapshot
authority or explicitly reclaim retained backups; the guard backs off without
classifying the mailbox as corrupt or stopping message writes.

A restart after an integrity incident writes new backups, which can rotate the
last pre-incident backup out (GH#278). Before restarting a server that reported
corruption, copy the backups somewhere outside the storage root:

```bash
db=$(am doctor locks --json | jq -r .database_path)   # read-only
mkdir -p ~/am-incident-backups
cp -p "$db".bak* ~/am-incident-backups/
```

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

Run `am doctor check` for a structured report (once A2 ships this):

```bash
am doctor check --format json | jq '.findings[] | select(.code == "GIT_2_51_0_INDEX_RACE")'
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
