# Mailbox Durability Audit

Bead: `br-97gc6.5.2`  
Date: `2026-04-18`  
Scope: DB/storage durability, automatic recovery, archive-truth handling, forensic capture, and operator-facing recovery clarity.

## Verdict

The durability program is substantially real already. The codebase has the right architectural spine for archive-first recovery: a runtime durability state machine, single-flight admission control with backoff/suppression, candidate-based reconstruction, quarantine-first mutation, and explicit forensic bundle capture.

The program is not fully sealed yet. Three gaps remain material:

1. A caller can establish an authoritative `storage_root`, but lower-level automatic recovery still reloads ambient config and can therefore recover against the wrong archive context.
2. The startup fast path for stale/corrupt WAL cleanup deletes `-wal` and `-shm` sidecars before the normal forensic-first recovery flow runs.
3. Startup surfaces expose excellent recovery context, but runtime/write-path failures still collapse to generic `am doctor ...` advice instead of surfacing the same root cause, mode, and artifact path.

## Confirmed Strengths

`crates/mcp-agent-mail-db/src/mailbox_verdict.rs:139` defines a concrete runtime durability machine with explicit read/write/recovery gates across `Healthy`, `DegradedReadOnly`, `Recovering`, and `Corrupt`. This directly matches the subtree doctrine that truthful degraded reads are preferable to alarming failures and that normal writes must stop outside `Healthy`.

`crates/mcp-agent-mail-db/src/pool.rs:395` implements recovery admission as a first-class subsystem. The controller combines single-flight, exponential backoff, and suppression-window logic, which directly addresses the plan-space concern about recovery thrash and retry storms.

`crates/mcp-agent-mail-db/src/forensics.rs:1` and `crates/mcp-agent-mail-db/src/pool.rs:4263` show that the normal repair/reconstruct path captures pre-recovery state and full forensic bundles before mutation. This is the correct basis for fail-closed recovery and post-incident diagnosis.

`crates/mcp-agent-mail-db/src/pool.rs:5037`, `crates/mcp-agent-mail-db/src/pool.rs:5077`, and `crates/mcp-agent-mail-db/src/pool.rs:5469` implement ownership refusal, candidate reconstruction, quarantine, and atomic promotion instead of mutating the live DB in place. `crates/mcp-agent-mail-db/src/pool.rs:5800` further confirms that archive-aware repair prefers backup restore, then archive reconstruction, and only falls through to blank reinitialize under tightly constrained conditions.

`crates/mcp-agent-mail-db/src/reconstruct.rs:747` computes an archive drift report from pre-mutation evidence, while `crates/mcp-agent-mail-server/src/startup_checks.rs:1439` enriches startup failures with recovery mode, owner, stall state, backlog pressure, next action, and forensic bundle location. This is already close to the desired operator experience.

## Findings

### 1. High: automatic recovery reloads ambient config instead of using caller-authoritative archive context

`DbPoolConfig` explicitly warns that leaving `storage_root` unset can reconcile against the wrong archive and alias unrelated mailboxes. That warning is documented in `crates/mcp-agent-mail-db/src/pool.rs:1647`.

Despite that contract, `recover_sqlite_file()` reconstructs its recovery context by calling `mcp_agent_mail_core::Config::from_env()` and then derives `storage_root` and `database_url` from ambient process state rather than from the caller's already-resolved pool config. The concrete behavior is in `crates/mcp-agent-mail-db/src/pool.rs:4259`.

This matters because `recover_sqlite_file()` is reached from startup integrity checks and runtime corruption recovery. If the live pool was created with an explicit `storage_root`, an ephemeral reroute, or another non-default mailbox context, the low-level repair path can make archive-recovery decisions using a different mailbox root than the caller intended.

Impact: incorrect archive association during automatic recovery is a correctness risk, not just a logging bug. In the best case, recovery falls back too aggressively. In the worse case, it reconciles or reconstructs against the wrong mailbox context.

Recommended fix: change `recover_sqlite_file()` to accept an explicit recovery context struct containing at least canonical DB path, authoritative `storage_root`, and redacted `database_url`. Ban env reload inside this path. Every caller that already knows the mailbox context should thread that context through directly.

### 2. Medium: startup WAL cleanup is not fully forensic-first and can discard evidence before normal recovery runs

The startup integrity probe contains a shortcut for stale/corrupt snapshot conflicts. When that path triggers, `try_wal_cleanup_and_retry()` calls `try_remove_corrupt_wal()` and deletes `-wal` and `-shm` sidecars before falling back to archive-aware recovery. The relevant logic is in `crates/mcp-agent-mail-server/src/startup_checks.rs:2161` and `crates/mcp-agent-mail-server/src/startup_checks.rs:2191`.

By contrast, the canonical archive-aware recovery path captures a pre-recovery snapshot and bundle before mutation and quarantines candidate artifacts instead of silently discarding them. That stronger behavior is visible in `crates/mcp-agent-mail-db/src/forensics.rs:79`, `crates/mcp-agent-mail-db/src/pool.rs:4263`, and `crates/mcp-agent-mail-db/src/pool.rs:5841`.

Impact: when the startup shortcut wins, the system can destroy the precise sidecar evidence that would have explained the incident or supported later salvage of DB-only state. That weakens the "forensic-first and non-destructive" doctrine and makes some failures less diagnosable than the main repair path.

Recommended fix: move snapshot-conflict handling onto the same forensic-first track as other recovery modes. If the fast path remains, it should first capture or quarantine sidecars into the forensic bundle before removing them from the live mailbox directory.

### 3. Low: recovery messaging is excellent at startup, but inconsistent on runtime and write-path failures

Startup failure formatting is strong. `crates/mcp-agent-mail-server/src/startup_checks.rs:1439` through `crates/mcp-agent-mail-server/src/startup_checks.rs:1704` append mode, phase, owner, stall reasons, deferred-write backlog, next action, and latest forensic bundle path.

The write-path and runtime-corruption surfaces do not expose the same context. `evaluate_write_route()` returns short refusal strings such as "Mailbox is {durability}" or "Another active process owns this mailbox" without bundle or mode details in `crates/mcp-agent-mail-db/src/pool.rs:1524`. Runtime corruption failure similarly returns generic manual-recovery guidance in `crates/mcp-agent-mail-db/src/pool.rs:2771`.

Impact: operators who hit durability trouble outside startup get less root-cause clarity, even though the system already knows more. This does not break durability correctness, but it weakens the "operator-invisible degradation, operator-visible root cause" goal.

Recommended fix: extract a shared recovery-context formatter usable by startup, write-path refusals, and runtime corruption errors. The rendered output should include durability mode, ownership state, recovery-lock state, and the latest forensic bundle when available.

## Recommended Follow-On Work

1. Thread authoritative mailbox context through all automatic recovery entrypoints and remove ambient config reads from DB-layer recovery helpers.
2. Make the startup WAL cleanup shortcut evidence-preserving by capturing or quarantining sidecars before deletion.
3. Unify startup/runtime/write-path recovery messaging around one formatter so every operator-facing failure surface explains mode, owner, and artifact location consistently.

## Closure Notes

This audit bead did not require code changes. The deliverable is the audit itself: a concrete assessment of what is already sealed, what remains open, and which fixes will most directly strengthen autonomous mailbox durability without broad refactoring.
