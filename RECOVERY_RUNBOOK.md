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
   am doctor fix-orphan-refs --all --dry-run --format json

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
cargo test -p mcp-agent-mail-storage --test libgit2_index_race_immunity \
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
