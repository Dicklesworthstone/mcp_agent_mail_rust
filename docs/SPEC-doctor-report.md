# SPEC: `am doctor` Report & Capabilities Contract

> Pinned in: `crates/mcp-agent-mail-cli/src/doctor/capabilities.rs::build_report::report_schema`
> First introduced: pass-7 of the world-class-doctor-mode-for-cli-tools methodology.
> Schema version: `1.0`. Doctor contract version: `1.0`.

This SPEC pins the JSON shapes that `am doctor` emits and the per-run
artifact layout it manages. Agents and tools that consume `am doctor`
output rely on this contract being stable across `doctor_version`
minor bumps. Breaking changes here REQUIRE a `doctor_contract_version`
major bump AND coordinated agent-side updates.

The contract is asserted at compile time by
`crates/mcp-agent-mail-cli/tests/doctor_capabilities_contract.rs` and
exercised end-to-end by `am doctor selftest`.

---

## 1. `am doctor capabilities --json`

Returns the agent-facing contract: detectors, fixers, exit codes, env
vars, write scopes, run-artifact layout. Agents read this once to
discover the doctor's surface programmatically.

**Shape:**

```jsonc
{
  "schema_version": "1.0",          // bumps with major contract changes
  "tool": "am",                      // always "am" for this binary
  "tool_version": "0.2.52",          // CARGO_PKG_VERSION at build time
  "doctor_version": "1.0.0",         // implementation version (minor for new fixers)
  "doctor_contract_version": "1.0",  // agent-facing contract (the pinned one)
  "platform": { "os": "linux", "arch": "x86_64" },
  "subsystems": ["db_state_files", "archive_state_files", ...],  // 11 subsystems
  "detectors": [
    {
      "id": "<detector-id>",         // e.g. "server_port" or "fm-..."
      "subsystem": "<subsystem>",    // one of the 11 subsystems
      "severity": "P0" | "P1" | "P2" | "P3",
      "description": "<one-line>",
      "estimated_cost_ms": 30,       // for budget calculations
      "online_required": false,
      "quick_mode_eligible": true    // included in `am doctor --quick`
    }
  ],
  "fixers": [
    {
      "id": "<fixer-id>",
      "preconditions": ["lock_acquired", "db_writable", ...],
      "writes_to": ["<path-or-pattern>"],
      "ops": ["WriteFile", "Rename", ...],  // canonical 7 only
      "reversible": true,
      "idempotent": true,
      "estimated_cost_ms": 50,
      "requires_yes": false           // true = needs --yes flag
    }
  ],
  "manual_remediations": [
    {
      "id": "<finding-id>",
      "instruction": "<what the user should do>",
      "reason": "<why doctor cannot auto-fix>"
    }
  ],
  "exit_codes": {
    "0":  "success_or_healthy",
    "1":  "findings_present_no_fix",
    "2":  "fix_partial",
    "3":  "fix_failed_rolled_back",
    "4":  "refused_unsafe",
    "5":  "concurrency_lost",
    "6":  "online_required",
    "64": "usage_error",
    "66": "no_input",
    "73": "cant_create",
    "74": "io_error"
  },
  "env_vars": {
    "AM_INTERFACE_MODE": "Must be 'cli' for am doctor",
    "AM_DOCTOR_BACKUPS_DIR": "Override default .doctor/ location",
    "AM_GIT_BINARY": "Alternate git binary",
    "AM_GIT_FLOCK_TIMEOUT_SECS": "Per-archive flock timeout, default 60",
    "STORAGE_ROOT": "Archive root override",
    "DATABASE_URL": "SQLite DB location override",
    "HTTP_BEARER_TOKEN": "Active bearer token",
    "NO_COLOR": "Disable ANSI",
    "AM_E2E_FORCE_LEGACY": "MUST NOT be set",
    "ALLOW_EPHEMERAL_PROJECTS_IN_DEFAULT_STORAGE": "/tmp project roots permitted"
  },
  "write_scopes": ["/abs/path/1", "/abs/path/2", ...],  // paths doctor may write to
  "run_artifact_layout": {
    "root": ".doctor/",
    "per_run_dir": ".doctor/runs/<ISO8601>__<run-id>/",
    "files": [
      "report.json", "report.md", "actions.jsonl", "scorecard.json",
      "stderr.log", "stdout.json", "undo.sh"
    ],
    "backups_dir": ".doctor/runs/<run-id>/backups/",
    "latest_symlink": ".doctor/latest -> runs/<run-id>",
    "history_jsonl": ".doctor/scorecard_history.jsonl"
  },
  "report_schema": "https://github.com/Dicklesworthstone/mcp_agent_mail_rust/blob/main/docs/SPEC-doctor-report.md"
}
```

### Stability

The following fields are **load-bearing contract**:
- `schema_version` (any change → contract major bump)
- `doctor_contract_version` (the pinned version itself)
- `tool` (always `"am"`)
- `subsystems` (the 11 from Phase 1 archaeology; adding/removing requires bump)
- `exit_codes` (the 11 codes; adding requires minor, removing requires major)
- The `Op` variant names referenced by `fixers[*].ops` (the canonical 7)
- `run_artifact_layout` keys

The following fields may grow without contract bump:
- `detectors[]` (additive only)
- `fixers[]` (additive only)
- `manual_remediations[]`
- `env_vars` (additive only)
- `write_scopes` (changes per machine state)

---

## 2. `report.json` (per-run artifact)

Written to `<repo>/.doctor/runs/<ISO8601>__<run-id>/report.json` by every
`am doctor` (default `check`) and `am doctor --fix` invocation.

**Shape:**

```jsonc
{
  "schema_version": "1.0",
  "tool": "am",
  "tool_version": "0.2.52",
  "doctor_version": "1.0.0",
  "run_id": "2026-05-10T12-34-56Z__a3f9b2",
  "run_dir": ".doctor/runs/2026-05-10T12-34-56Z__a3f9b2",
  "started_at": "2026-05-10T12:34:56Z",
  "finished_at": "2026-05-10T12:34:56.412Z",
  "duration_ms": 412,
  "target_sha": "deadbeef0123...",
  "ok": false,
  "summary": {
    "total_findings": 3,
    "by_severity": { "P0": 1, "P1": 0, "P2": 2, "P3": 0 },
    "auto_fixable": 3,
    "online_required": 0
  },
  "findings": [
    {
      "id": "<detector-id>",
      "severity": "P0" | "P1" | "P2" | "P3",
      "subsystem": "<subsystem>",
      "title": "<one-line>",
      "confidence": 1.0,
      "evidence": {
        "file": "<path>",
        "lines": [42, 43],
        "query": "<sql or pattern>",
        "hash": "sha256:..."
      },
      "remediation": {
        "command": "am doctor --fix --only <id>",
        "explain_command": "am doctor explain <id>",
        "auto_fixable": true,
        "estimated_actions": 2
      }
    }
  ],
  "exit_code": 1,
  "next_steps": [
    "Run: am doctor --fix",
    "Or scope: am doctor --fix --only <id>",
    "Inspect: am doctor explain <id>"
  ]
}
```

For `--fix` runs, the report adds:
- `actions_jsonl_path`
- `backups_dir`
- `undo_command`
- `summary.actions_taken`
- `summary.bytes_backed_up`

---

## 3. `actions.jsonl` (one line per `mutate()` call)

Append-only per-run log. Pass-5 introduces the **two-phase write protocol**:
each mutation produces TWO lines, distinguished by `phase`:

### Pending line (written BEFORE the mutation executes)

```jsonc
{
  "path": "<rel-path-from-repo-root>",
  "op": "WriteFile" | "AppendFile" | "Rename" | "Chmod" | "DbExec" | "DbMigrate" | "SymlinkAtomic",
  "before_hash": "sha256:...",
  "after_hash": "",                 // unknown until step 8
  "started_at_ns": 12345000000,     // monotonic since MutateContext.start
  "finished_at_ns": 0,              // not yet finished
  "run_id": "...",
  "fixer_id": "...",
  "ok": false,                       // mutation hasn't executed yet
  "phase": "pending",                // ← key marker
  "before_mode": 420,                // optional
  "rename_to": "<dest>"              // optional, only for Rename
}
```

### Completed line (written AFTER the mutation succeeds or rolls back)

```jsonc
{
  "path": "...",
  "op": "...",
  "before_hash": "sha256:...",
  "after_hash": "sha256:...",        // post-mutation hash
  "started_at_ns": 12345000000,      // matches the pending line
  "finished_at_ns": 12345120000,
  "run_id": "...",
  "fixer_id": "...",
  "ok": true,                         // or false if exec failed
  "phase": "completed",               // ← matched with pending via started_at_ns
  "before_mode": 420,
  "after_mode": 384,
  "rolled_back": null,                // present if ok=false; true if rollback succeeded
  "rename_to": "...",
  "error": "..."                      // present if ok=false
}
```

### Crash-window semantics

If only a `pending` line exists (no `completed` line with matching
`started_at_ns`), the mutation may have completed but the log was
truncated by SIGINT/panic/poweroff. The verbatim backup at
`backups/seq_<started_at_ns>/<rel>/` is the source of truth; undo
restores from it.

### Per-mutation backup layout

Pass-4 fix: each mutation's backup lives under its own seq directory.

```
.doctor/runs/<run-id>/
├── actions.jsonl
└── backups/
    ├── seq_00000000000000012345000000/    ← first mutation
    │   └── alpha.txt                       ← pre-mutation copy
    ├── seq_00000000000000023456000000/    ← second mutation
    │   └── alpha.txt                       ← (different content from above)
    └── seq_00000000000000034567000000/    ← third mutation
        └── beta.txt
```

This prevents two mutations to the same path from overwriting each
other's backups (caught by pass-4's property test).

---

## 4. `am doctor selftest`

Pass-6 verb. End-to-end exercise of the chokepoint primitives in an
isolated tempdir. Reports JSON envelope:

```jsonc
{
  "schema_version": "1.0",
  "doctor_version": "1.0.0",
  "doctor_contract_version": "1.0",
  "tool": "am",
  "tool_version": "0.2.52",
  "ok": true,
  "checks": [
    {"name": "write_file_mutation",            "ok": true},
    {"name": "append_file_mutation",           "ok": true},
    {"name": "chmod_mutation",                 "ok": true},
    {"name": "rename_mutation",                "ok": true},
    {"name": "per_mutation_seq_backups",       "ok": true, "seq_dir_count": 4},
    {"name": "undo_round_trip_byte_identical", "ok": true,
     "actions_replayed": 4, "failures": []}
  ],
  "duration_ms": 1,
  "tempdir": "/tmp/.tmpXXXXXX"
}
```

Exit 0 on pass, 1 on fail. For operators after install/upgrade.

---

## 5. Versioning policy

- **`tool_version`** (`am --version`) — semver of the binary; bumps with any change.
- **`doctor_version`** — implementation version; minor for new fixers/detectors, major for incompatible refactors. Independent of `tool_version`.
- **`doctor_contract_version`** — agent-facing surface. Bumps require coordinated agent-side updates.

Agents should pin against `doctor_contract_version`. The capabilities
report's `schema_version` is == `doctor_contract_version` today; they
may diverge in the future for purely additive schema changes.

---

## 6. Cross-references

- `crates/mcp-agent-mail-cli/src/doctor/mutate.rs` — chokepoint
- `crates/mcp-agent-mail-cli/src/doctor/runs.rs` — per-run artifact dir
- `crates/mcp-agent-mail-cli/src/doctor/capabilities.rs` — capabilities --json builder
- `crates/mcp-agent-mail-cli/src/doctor/undo.rs` — undo replay engine
- `crates/mcp-agent-mail-cli/tests/doctor_capabilities_contract.rs` — snapshot test
- `crates/mcp-agent-mail-cli/tests/doctor_property_round_trip.rs` — randomized round-trip
- `world-class-doctor-mode-for-cli-tools/references/methodology/CLI-SURFACE.md` — verbatim CLI spec
- `world-class-doctor-mode-for-cli-tools/references/methodology/MUTATE-CHOKEPOINT.md` — chokepoint contract
- `world-class-doctor-mode-for-cli-tools/references/methodology/OUTPUT-SCHEMA.md` — per-run artifact schema

## 7. AGENTS.md anchors

Per `AGENTS.md`:
- **RULE 1** — No file deletion. `Op::Rename` to quarantine instead.
- **RULE 2** — No broadcast messaging. Doctor never invokes `send_message`.
- "Irreversible Git & Filesystem Actions" — No `rm -rf`, `git reset --hard`, `git clean -fd`.
- "Async Runtime: asupersync (MANDATORY — NO TOKIO)" — `mutate()` is intentionally synchronous.
