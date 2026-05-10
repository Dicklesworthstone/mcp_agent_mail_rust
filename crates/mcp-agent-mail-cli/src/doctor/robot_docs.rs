//! `am doctor robot-docs` — paste-ready agent handbook.
//!
//! When an agent invokes `am doctor` cold (no prior context), this is the
//! single command that should make the rest of the surface obvious.
//! Output is Markdown to stdout; auto-disables ANSI on non-TTY.
//!
//! Per the world-class-doctor-mode kernel (Axiom 0: first-try success),
//! the goal is for an agent to read this once and never need to read
//! source code or methodology files to use the doctor effectively.

#![forbid(unsafe_code)]

/// The full paste-ready handbook. Includes:
/// - One-paragraph orientation
/// - The 10 verb table
/// - The 11-code exit table
/// - The 5 most common workflows
/// - Pointers to capabilities + JSON shapes
/// - The two AGENTS.md absolutes that affect doctor behavior
pub fn handbook() -> &'static str {
    HANDBOOK_TEXT
}

const HANDBOOK_TEXT: &str = r#"# `am doctor` — Agent Handbook

You are an AI coding agent. The Agent Mail mailbox you depend on may have
drifted; `am doctor` is how you find out and how you fix it. The handbook
below is the single source of truth — you should not need to read source
or run other commands to use the doctor effectively.

## Orientation

`am doctor` diagnoses (and, with `--fix`, repairs) Agent Mail's mailbox
state: SQLite DB, Git-backed archive, MCP client configs, pre-commit guard,
runtime listener, environment, share/atc/search/identity state. Every
mutation is **backed up first**, **hash-witnessed**, and **reversible via
`am doctor undo <run-id>`**. The doctor never deletes user files; it
quarantines via rename.

## The 10 Verbs

| Verb | Purpose | Mutates? | Default exit |
|------|---------|----------|--------------|
| `am doctor` (or `check`) | Run all detectors. Read-only. | No | 0 healthy / 1 findings |
| `am doctor --fix` | Run detectors + apply fixers. Backups first. | Yes (via `mutate()`) | 0 / 2 / 3 / 4 |
| `am doctor --dry-run --fix` | Print the fix plan; do not execute. | No | 0 |
| `am doctor undo <run-id>` | Restore from `.doctor/runs/<run-id>/backups/`. | Yes (restore-only) | 0 / 3 |
| `am doctor capabilities --json` | Print machine-readable contract. | No | 0 |
| `am doctor robot-docs` | This handbook. | No | 0 |
| `am doctor health` | One-line liveness summary. For CI. | No | 0 / 1 |
| `am doctor ls` | List `.doctor/runs/` entries. | No | 0 |
| `am doctor diff [<ref>]` | Show what `--fix` would change. Read-only. | No | 0 |
| `am doctor gc --before <date> --yes` | Prune old run dirs. Requires both flags. | Yes (deletion) | 0 |

Plus existing verbs preserved for backward compat: `repair`, `backups`,
`restore`, `reconstruct`, `archive-scan`, `archive-verify`,
`archive-normalize`, `fix`,
`fix-orphan-refs`, `pack-archive`.

## Exit codes

| Code | Name | When |
|------|------|------|
| 0 | success_or_healthy | Clean diagnose, fix complete, undo complete |
| 1 | findings_present_no_fix | Diagnose found issues; `--fix` is recommended |
| 2 | fix_partial | `--fix`: some fixed, some not (see `report.json::partial_failures`) |
| 3 | fix_failed_rolled_back | At least one mutation failed; rolled back |
| 4 | refused_unsafe | State unsafe (schema mismatch, scope violation, unmet precondition) |
| 5 | concurrency_lost | Another doctor invocation holds the lock |
| 6 | online_required | At least one finding needs `--online`; not passed |
| 64 | usage_error | Unknown flag / missing arg (POSIX EX_USAGE) |
| 66 | no_input | Target path doesn't exist or isn't a recognized project |
| 73 | cant_create | Couldn't create `.doctor/runs/<run-id>/` |
| 74 | io_error | Filesystem I/O during read or non-mutating write |

## Five Recipes (Copy-Paste Ready)

### 1. Healthy-baseline triage (start of session)

```bash
am doctor --json | jq -e '.ok'
```

Returns `true` healthy, `false` with findings. If false:

```bash
am doctor --json | jq '.findings[] | {id, severity, title}'
```

### 2. Plan-then-fix workflow

```bash
am doctor --dry-run --fix          # preview
am doctor --fix                    # apply with backups
am doctor                          # confirm exit 0
```

### 3. Reverse a fix that went wrong

```bash
am doctor undo latest              # most recent
# or:
am doctor ls                       # see all runs
am doctor undo 2026-05-09T16-30-15Z__abc123
```

### 4. Pre-commit fast path

```bash
am doctor --quick --json           # < 200ms; only fast detectors
```

Use as a pre-commit gate; fail if exit 1.

### 5. Targeted scope (one finding at a time)

```bash
am doctor --json | jq -r '.findings[0].id'           # get a finding id
am doctor --explain <finding-id>                      # see evidence
am doctor --fix --only <finding-id>                   # apply just that fix
```

## Per-run artifacts

Every `--fix` run creates `.doctor/runs/<ISO>__<run-id>/`:

```
.doctor/runs/2026-05-09T16-30-15Z__abc123/
├── report.json           # findings + summary; same shape as `--json` output
├── report.md             # human-readable narrative
├── actions.jsonl         # one line per mutate() call (before/after hashes)
├── backups/              # verbatim per-file copies (preserves perms, mtime)
├── stderr.log
├── stdout.json
└── undo.sh               # idempotent shell script wrapping `am doctor undo`
```

`.doctor/latest` is an atomic symlink to the most recent run.

`.doctor/scorecard_history.jsonl` is the per-run trend timeseries (one line
per run, ordered by start time).

`.doctor/` is added to `.gitignore` automatically on first run.

## Hard guarantees (kernel axioms applied)

- **Detect-then-fix**: detectors are pure; nothing writes without `--fix`.
- **Single chokepoint**: every disk write under `--fix` flows through
  one `mutate()` function. Verified by `validate-doctor.sh`.
- **Backup before mutation**: `mutate()` writes a verbatim backup BEFORE
  changing anything. `cmp_strict(backup, live)` succeeds at backup time.
- **Hash witness**: every mutation records `{path, op, before_hash,
  after_hash, started_at_ns, finished_at_ns, run_id, fixer_id, ok}` in
  `actions.jsonl`. SHA-256.
- **Reversible**: `undo <run-id>` reads `actions.jsonl` in reverse,
  restores from `backups/`, verifies hash. Fails closed if any backup
  is missing.
- **Idempotent**: `--fix` then `--fix` → second run reports `actions_taken: 0`.
- **Concurrency-safe**: two `--fix` invocations → one wins, the other
  refuses with exit 5.
- **Crash-recoverable**: SIGKILL mid-fix → next run finishes or aborts
  cleanly. Atomic write-tmp-rename throughout.
- **Read-only by default**: bare `am doctor` never mutates state.
- **Stable JSON schema**: `--json` always includes `schema_version`.
- **Stdout = data, stderr = progress**: `--json | jq` is always safe.
- **Offline by default**: network probes opt-in via `--online`.

## What `am doctor --fix` will NOT do (per AGENTS.md + safety envelope)

- Delete user files (rename to `<run-dir>/quarantine/<rel>` instead).
- Run `rm -rf`, `git reset --hard`, `git clean -fd`.
- Edit your shell rc files (`~/.bashrc`, `~/.zshrc`, etc.) — emits a finding.
- Modify canonical mail messages under `<storage_root>/projects/<slug>/messages/`.
- Touch `~/.gitconfig` or `~/.git-credentials`.
- Send any `send_message` call (broadcast or otherwise — Rule 2 of AGENTS.md).
- Probe network unless `--online` is set.
- Mutate while another doctor invocation holds the lock.

## Capabilities (machine-readable contract)

```bash
am doctor capabilities --json | jq '.detectors | length'   # 30+
am doctor capabilities --json | jq '.fixers | length'
am doctor capabilities --json | jq '.exit_codes | keys'    # ["0","1","2","3","4","5","6","64","66","73","74"]
am doctor capabilities --json | jq '.subsystems'           # 11 subsystems
am doctor capabilities --json | jq '.write_scopes'         # paths doctor may touch
```

## Subsystem reference

The 11 subsystems doctor covers (each has its own detectors/fixers):

1. `db_state_files` — SQLite DB, WAL/SHM, schema, FTS, search V3 index
2. `archive_state_files` — Git archive, project.json, locks, refs/objects
3. `runtime_processes` — listener, port, supervisor, PID hints
4. `mcp_config_files` — Claude/Codex/Gemini/Cursor/Cline/etc. configs
5. `secrets_env_state` — bearer tokens, JWT keys, env files
6. `guard_install` — pre-commit hook integrity, archive read, rename handling
7. `environment_toolchain` — git version, PATH, installed agents
8. `share_export_state` — share bundles, scrub, manifests, signatures
9. `atc_learning_state` — ATC durability, write_mode, rollups
10. `search_index_state` — Search V3 / frankensearch index hygiene
11. `identity_contacts_state` — agents, contacts, build_slots, pane identity

## When stuck: meta-recovery

If `am doctor` itself doesn't run (binary missing, locked):

```bash
# 1. Check the lock
cat <repo>/.doctor/.doctor.lock 2>/dev/null
# fs2 advisory lock dies with the holding process; if held, find that process.

# 2. Read the latest run manually
cat <repo>/.doctor/latest/report.json

# 3. Replay actions.jsonl in reverse without `am doctor undo`
tac <repo>/.doctor/runs/<id>/actions.jsonl | while read line; do
  path=$(echo "$line" | jq -r .path)
  cp "<repo>/.doctor/runs/<id>/backups/$path" "<repo>/$path"
done
```

## Versioning

- `tool_version` — am binary semver
- `doctor_version` — implementation version (minor for new fixers)
- `doctor_contract_version` — agent-facing contract (major-bump on breaks)

You only need to track `doctor_contract_version`. Read it from
`am doctor capabilities --json | jq -r .doctor_contract_version`.

---

For deeper documentation, see:
- `am doctor capabilities --json` — machine-readable contract
- `<repo>/.doctor/runs/<id>/report.md` — human narrative for the latest run
- The repo's `AGENTS.md` (Rules 0/1/2 are absolute prohibitions)
- The repo's `docs/RECOVERY_RUNBOOK.md` (when present)
- The repo's `docs/OPERATOR_RUNBOOK.md` (when present)
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handbook_contains_all_canonical_verbs() {
        let h = handbook();
        for verb in [
            "am doctor",
            "--fix",
            "--dry-run",
            "undo",
            "capabilities",
            "robot-docs",
            "health",
            "ls",
            "diff",
            "gc",
        ] {
            assert!(h.contains(verb), "handbook missing verb: {}", verb);
        }
    }

    #[test]
    fn handbook_documents_all_exit_codes() {
        let h = handbook();
        for code in ["0", "1", "2", "3", "4", "5", "6", "64", "66", "73", "74"] {
            assert!(h.contains(code), "handbook missing exit code: {}", code);
        }
    }

    #[test]
    fn handbook_lists_all_11_subsystems() {
        let h = handbook();
        for s in [
            "db_state_files",
            "archive_state_files",
            "runtime_processes",
            "mcp_config_files",
            "secrets_env_state",
            "guard_install",
            "environment_toolchain",
            "share_export_state",
            "atc_learning_state",
            "search_index_state",
            "identity_contacts_state",
        ] {
            assert!(h.contains(s), "handbook missing subsystem: {}", s);
        }
    }

    #[test]
    fn handbook_mentions_no_destructive_shell() {
        let h = handbook();
        assert!(h.contains("rm -rf"), "should warn against rm -rf");
        assert!(h.contains("AGENTS.md"), "should reference AGENTS.md");
    }
}
