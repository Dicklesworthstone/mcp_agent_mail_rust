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
/// - Every `am doctor` verb, split into diagnose and repair tables
/// - The 11-code exit table
/// - Copy-paste workflows (baseline, plan-then-fix, undo, pre-commit,
///   targeted, per-FM, list-all)
/// - Pointers to capabilities + JSON shapes
/// - The two AGENTS.md absolutes that affect doctor behavior
pub fn handbook() -> &'static str {
    HANDBOOK_TEXT
}

/// Markdown section documenting every Air Traffic Control (`AM_ATC_*`)
/// variable: name, kind, accepted values, default, restart requirement, and
/// meaning. Generated from `mcp_agent_mail_core::flags::FLAG_REGISTRY` so the
/// handbook and the binary cannot disagree (GH#290). Defaults are printed
/// rather than effective values because this text is meant to be pasted into
/// agent context; run `am config atc` for the live effective configuration.
#[must_use]
pub fn atc_configuration_section() -> String {
    use mcp_agent_mail_core::flags::{FlagKind, flag_registry};
    use std::fmt::Write as _;

    let mut out = String::from(
        "## Air Traffic Control (ATC) configuration\n\n\
ATC reviews agent liveness, detects reservation deadlocks, and can send probe/advisory \
mail or reclaim reservations. It is on by default, but the executor defaults to `shadow`, \
so nothing durable is written unless `AM_ATC_EXECUTOR_MODE` is set to `canary`/`live`. \
All variables are read at server startup unless noted. Inspect effective values and their \
sources with `am config atc` (alias: `am flags list --subsystem atc`); `am flags explain <VAR>` \
prints one variable in full.\n\n\
| Variable | Kind | Accepted | Default | Restart | Meaning |\n\
|---|---|---|---|---|---|\n",
    );
    for flag in flag_registry()
        .iter()
        .filter(|flag| flag.subsystem == "atc" || flag.env_var == "ATC_LEARNING_DISABLED")
    {
        let accepted = match flag.kind {
            FlagKind::Bool | FlagKind::Enum(_) => flag.kind.allowed_values().join(" / "),
            FlagKind::Integer => "integer (see meaning for floor/ceiling)".to_string(),
            FlagKind::Float => "finite float (see meaning for range)".to_string(),
            FlagKind::Path => "filesystem path".to_string(),
            FlagKind::Text => "text".to_string(),
        };
        let restart = if flag.restart_required { "yes" } else { "no" };
        let mut meaning = flag.doc.replace('|', "\\|");
        if let Some(notes) = flag.notes {
            let _ = write!(meaning, " Notes: {}", notes.replace('|', "\\|"));
        }
        let _ = writeln!(
            out,
            "| `{}` | {} | {} | `{}` | {} | {} |",
            flag.env_var,
            flag.kind.label(),
            accepted,
            flag.default_value,
            restart,
            meaning
        );
    }
    out.push_str(
        "\nSetting `AM_ATC_ENABLED=false` means \"passive liveness only\": the engine ignores \
every hook and the operator loop never starts, so there are no probes, advisories, reservation \
releases, or experience rows; liveness views fall back to the database `last_active_ts` that \
ordinary tool calls write, and reservations of crashed agents expire only by TTL.\n",
    );
    out
}

const HANDBOOK_TEXT: &str = r#"# `am doctor` — Agent Handbook

You are an AI coding agent. The Agent Mail mailbox you depend on may have
drifted; `am doctor` is how you find out and how you fix it. The handbook
below is the single source of truth — you should not need to read source
or run other commands to use the doctor effectively.

## Orientation

`am doctor` diagnoses (and, through `am doctor fix`, repairs) Agent Mail's
mailbox state: SQLite DB, Git-backed archive, MCP client configs, pre-commit
guard, runtime listener, environment, share/atc/search/identity state. Every
mutation is **backed up first**, **hash-witnessed**, and **reversible via
`am doctor undo <run-id>`**. The doctor never deletes user files; it
quarantines via rename.

`am doctor` always takes a verb: bare `am doctor` prints usage and exits 2,
and every flag belongs to a verb (`am doctor check --json`, never
`am doctor --json`). Mutating verbs refuse to prompt on a non-interactive
stdin, so an agent passes `--yes` (or `--dry-run`) explicitly.

## Verbs

Diagnose (read-only):

| Verb | Purpose | Default exit |
|------|---------|--------------|
| `am doctor check` (`--json`, `--verbose`, `[PROJECT]`) | Run every mailbox check. The table form exits 1 when a check fails; `--json` always exits 0 — branch on `.healthy`. | 0 / 1 (table) |
| `am doctor health` | One-line verdict from the live mailbox plus the latest run history. For CI and pre-commit hooks. | 0 healthy / 1 findings |
| `am doctor triage` (`--quick`) | One JSON envelope: live mailbox probe (`live_health`), the latest run report, planned actions, and a `recommended_command`. Never reports all-clear during an active live failure. | 0 |
| `am doctor fix --list` (`--json`) | Run every registered FM detector in one round-trip; `per_fm[]`, `skipped[]`, `total_findings`. | 0 |
| `am doctor fix --only <fm-id> --list` | Run one FM's detector — no chokepoint, no run dir. | 0 |
| `am doctor locks` (`--json`) | Activity locks, holder PIDs, and the owner class (`live` / `wedged` / `reclaimable` / `stale`) with a safe next command. | 0 |
| `am doctor drain` (`--json`) | Whether mutating doctor work is safe right now (`safe_to_mutate`) and the supervised drain steps when a live owner is present. Never kills `am`. | 0 |
| `am doctor archive-scan` | Audit archive hygiene. | see `--help` |
| `am doctor archive-verify` | Cross-check archive artifacts against SQLite for tamper evidence. | see `--help` |
| `am doctor backups` (`--json`) | List the SQLite backups available to `restore`. | 0 |
| `am doctor artifacts` | Inventory test/perf/forensic/e2e artifact roots. Deletes nothing. | 0 |
| `am doctor ls` | List `.doctor/runs/` entries. | 0 |
| `am doctor explain <id>` | Drill into one finding (latest run) or one registered FM (registry fallback). | 0 / 64 |
| `am doctor fixers` | List every per-FM detector+fixer pair in the registry. | 0 |
| `am doctor capabilities --json` | Machine-readable contract (detectors, fixers, fm_fixers, exit codes, env vars, write scopes). | 0 |
| `am doctor robot-docs` | This handbook. | 0 |
| `am doctor selftest` | Exercise the `mutate()` primitives end-to-end in a tempdir. | 0 / 1 |
| `am doctor mcp-selftest` | MCP JSON-RPC decode + dispatch self-test in an isolated scratch mailbox. | 0 / 1 |
| `am doctor write-selftest` | Real write-path self-test in an isolated scratch mailbox. | 0 / 1 |
| `am doctor support-bundle` (`--json`) | Write a sanitized incident bundle for maintainers (the bundle is its only write). | 0 |

Repair (mutating; preview first with `--dry-run`):

| Verb | Purpose | Default exit |
|------|---------|--------------|
| `am doctor fix --only <fm-id> --yes` | Apply one FM's fix through the `mutate()` chokepoint. Exit equals the envelope's `exit_code`: 1 = findings remain, nothing mutated; 2 = partial fix. | 0 / 1 / 2 / 3 / 4 / 64 |
| `am doctor fix --yes` | The legacy multi-detector flow (shell rc aliases, PATH order, MCP configs, stale `.git/index.lock`, guard hooks, database repair/reconstruct, an unhealthy local server, WAL mode). Prefer `--only`. | 0 / 2 / 3 / 4 |
| `am doctor undo <run-id>` | Restore from `.doctor/runs/<run-id>/backups/`. | 0 / 3 |
| `am doctor repair` | Repair the SQLite index in place. Refuses (exit 3) while a live owner holds the mailbox. | 0 / 3 |
| `am doctor reconstruct` | Rebuild the SQLite index from the Git archive. Same owner guard as `repair`. | 0 / 3 |
| `am doctor restore <backup-path>` | Restore the database from a backup listed by `backups`. | see `--help` |
| `am doctor vacuum` | VACUUM + ANALYZE the live database in place (orphaned pages). Same owner guard. | see `--help` |
| `am doctor reclaim` | Consolidate stale recovery debris into one reversible directory. Previews without `--yes`. | 0 |
| `am doctor archive-normalize` | Quarantine or annotate anomalous archive files. Never deletes. | see `--help` |
| `am doctor fix-orphan-refs` | Report refs whose objects are missing; prunes (with backups) only with `--apply`. | see `--help` |
| `am doctor pack-archive` | Git loose-object repack of the archive (`--plan` only inspects). | see `--help` |

When a live owner blocks `repair` / `reconstruct` / `vacuum`, drain it
through its supervisor and confirm with `am doctor drain`; pass
`--take-ownership` only for an owner `am doctor locks` classifies as
`reclaimable`. Doctor never kills `am`.

## Exit codes

| Code | Name | When |
|------|------|------|
| 0 | success_or_healthy | Clean diagnose, fix complete, undo complete |
| 1 | findings_present_no_fix | Diagnose found issues; a fix is recommended |
| 2 | fix_partial | `fix`: some fixed, some not (see `report.json::partial_failures`) |
| 3 | fix_failed_rolled_back | At least one mutation failed; rolled back |
| 4 | refused_unsafe | State unsafe (schema mismatch, scope violation, unmet precondition) |
| 5 | concurrency_lost | Another doctor invocation holds the lock |
| 6 | online_required | Reserved; no verb takes `--online` today |
| 64 | usage_error | Arguments parse but name nothing valid: unknown FM or finding id, missing FM input (POSIX EX_USAGE). A flag or verb the parser rejects exits 2 |
| 66 | no_input | Target path doesn't exist or isn't a recognized project |
| 73 | cant_create | Couldn't create `.doctor/runs/<run-id>/` |
| 74 | io_error | Filesystem I/O during read or non-mutating write |

## Recipes (Copy-Paste Ready)

### 1. Healthy-baseline triage (start of session)

```bash
am doctor check --json | jq -e '.healthy'
```

Prints `true` (exit 0) when healthy, `false` (exit 1, from `jq -e`)
otherwise. If false:

```bash
am doctor check --json | jq '.checks[] | select(.status != "ok") | {check, status, detail}'
am doctor fix --list --json | jq '.per_fm[] | select(.findings_count > 0) | {fm_id, severity, findings_count}'
```

### 2. Plan-then-fix workflow

```bash
am doctor fix --only <fm-id> --dry-run       # rehearse through the chokepoint
am doctor fix --only <fm-id> --yes           # apply with backups
am doctor fix --only <fm-id> --list --json   # confirm findings_count is 0
```

### 3. Reverse a fix that went wrong

```bash
am doctor undo latest --dry-run    # print the restore plan
am doctor undo latest              # most recent
# or:
am doctor ls                       # see all runs
am doctor undo 2026-05-09T16-30-15Z__abc123
```

### 4. Pre-commit fast path

```bash
am doctor health                   # one line; exit 1 when findings are present
```

Use as a pre-commit gate; fail if exit 1. `am doctor triage --quick` is
the JSON equivalent for agents (it always exits 0; read
`recommended_command`).

### 5. Targeted scope (one finding at a time)

```bash
am doctor fix --list --json | jq -r '.per_fm[] | select(.findings_count > 0) | .fm_id'
am doctor explain <fm-id>                             # see evidence
am doctor fix --only <fm-id> --yes                    # apply just that fix
```

`am doctor explain` falls back to the registry when no recent
run includes the id, so `am doctor explain <fm-id>` works cold —
useful for understanding what an FM does before invoking it.

### 6. Per-FM surface (recommended for agents)

```bash
am doctor fixers --format json | jq '.fixers[].id'   # enumerate registered FMs
am doctor fix --list --json                           # detect across every FM
am doctor fix --only <fm-id> --list --json            # preview one FM's findings
am doctor fix --only <fm-id> --dry-run                # rehearse through chokepoint
am doctor fix --only <fm-id> --yes                    # apply that one FM's fix
am doctor undo latest                                 # rollback if needed
```

The per-FM verbs route every mutation through the `mutate()`
chokepoint: verbatim backups in `<run-dir>/backups/seq_<ns>/`,
hash-witnessed actions in `actions.jsonl`, reversible via undo.
The legacy `am doctor fix` (without `--only`) runs the older
multi-detector flow; prefer the per-FM verbs when targeting
specific failure modes.

### 7. One-shot system survey

```bash
am doctor fix --list --json | jq '{
  total: .total_findings,
  by_severity: (.per_fm | group_by(.severity) | map({(.[0].severity): map(.findings_count) | add}) | add),
  per_fm: (.per_fm | map(select(.findings_count > 0)) | map({fm_id, findings_count, actions_planned})),
  skipped: .skipped
}'
```

Single round-trip: every registered FM's detector runs, findings
aggregate, FMs missing required inputs (e.g., git not on PATH for
known-bad-git, or `:memory:` DB URL for storage-db chmod) are
surfaced in `skipped[]` with the missing field name.

## Per-run artifacts

Every `fix --only` (or `archive-normalize`) run that changes something
creates `.doctor/runs/<ISO>__<run-id>/`:

```
.doctor/runs/2026-05-09T16-30-15Z__abc123/
├── report.json           # the run's JSON envelope (summary + outcome)
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

- **Detect-then-fix**: detectors are pure; nothing writes without `fix`.
- **Single chokepoint**: every disk write under `fix --only` flows through
  one `mutate()` function. Verified by `validate-doctor.sh`.
- **Backup before mutation**: `mutate()` writes a verbatim backup BEFORE
  changing anything. `cmp_strict(backup, live)` succeeds at backup time.
- **Hash witness**: every mutation records `{path, op, before_hash,
  after_hash, started_at_ns, finished_at_ns, run_id, fixer_id, ok}` in
  `actions.jsonl`. SHA-256.
- **Reversible**: `undo <run-id>` reads `actions.jsonl` in reverse,
  restores from `backups/`, verifies hash. Fails closed if any backup
  is missing.
- **Idempotent**: the same `fix --only <fm-id> --yes` twice → the second
  run reports `actions_taken: 0`.
- **Concurrency-safe**: two `fix` invocations → one wins, the other
  refuses with exit 5.
- **Crash-recoverable**: SIGKILL mid-fix → next run finishes or aborts
  cleanly. Atomic write-tmp-rename throughout.
- **Read-only by default**: the diagnose verbs never mutate state.
- **Stable JSON schema**: `--json` always includes `schema_version`.
- **Stdout = data, stderr = progress**: `--json | jq` is always safe.
- **Offline**: the only network I/O is a loopback probe of the local
  Agent Mail listener.

## What `am doctor fix --only` will NOT do (per AGENTS.md + safety envelope)

- Delete user files (rename to `<run-dir>/quarantine/<rel>` instead).
- Run `rm -rf`, `git reset --hard`, `git clean -fd`.
- Edit your shell rc files (`~/.bashrc`, `~/.zshrc`, etc.) — emits a finding.
  The legacy `am doctor fix` flow is the exception: it comments out Python
  aliases and appends `~/.local/bin` to PATH in rc files, so preview it
  with `am doctor fix --dry-run`.
- Modify canonical mail messages under `<storage_root>/projects/<slug>/messages/`.
- Touch `~/.gitconfig` or `~/.git-credentials`.
- Send any `send_message` call (broadcast or otherwise — Rule 2 of AGENTS.md).
- Reach anything beyond the loopback listener probe.
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
            "am doctor fix",
            "--dry-run",
            "--only",
            "undo",
            "capabilities",
            "fixers",
            "explain",
            "robot-docs",
            "health",
            "ls",
            "triage",
            "selftest",
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
    fn atc_configuration_section_lists_every_registered_atc_variable() {
        let section = atc_configuration_section();
        assert!(section.contains("## Air Traffic Control (ATC) configuration"));
        assert!(section.contains("passive liveness only"));
        for flag in mcp_agent_mail_core::flags::flag_registry()
            .iter()
            .filter(|flag| flag.subsystem == "atc")
        {
            let row_prefix = format!("| `{}` |", flag.env_var);
            assert!(
                section.contains(&row_prefix),
                "ATC section missing {}",
                flag.env_var
            );
            assert!(
                section.contains(&format!("| `{}` |", flag.default_value)),
                "ATC section missing default for {}",
                flag.env_var
            );
        }
        assert!(section.contains(
            "| `AM_ATC_EXECUTOR_MODE` | enum | shadow / dry_run / canary / live | `shadow` | yes |"
        ));
        assert!(section.contains("| `ATC_LEARNING_DISABLED` |"));
    }

    #[test]
    fn handbook_mentions_no_destructive_shell() {
        let h = handbook();
        assert!(h.contains("rm -rf"), "should warn against rm -rf");
        assert!(h.contains("AGENTS.md"), "should reference AGENTS.md");
    }
}
