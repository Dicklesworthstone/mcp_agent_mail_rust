# SPEC: Ephemeral Root Classification and Default Mailbox Safety Policy

**Status:** Draft
**Date:** 2026-04-02
**Authors:** RubyKnoll (claude-code/opus-4.6)
**Bead:** br-97gc6.5.2.2.1

## Summary

This specification defines which project roots and execution contexts are classified as "ephemeral" and what safe default behaviors apply to each class.

## Goals

1. Prevent accidental corruption of the production mailbox archive by test/repro runs
2. Isolate ephemeral sessions automatically without requiring explicit user action
3. Provide explicit opt-in for intentional ephemeral access to the shared mailbox
4. Enable deterministic, discoverable, and self-cleaning isolated storage roots

## Non-Goals

- Defining the isolated storage root location algorithm
- Specifying cleanup schedules or retention policies
- Performance optimization of ephemeral storage

---

## Ephemeral Root Classification

### 1. Explicit Ephemeral Markers

| Pattern | Classification | Confidence |
|---------|---------------|------------|
| `/tmp/` prefix | Ephemeral | High |
| `/var/tmp/` prefix | Ephemeral | High |
| `/dev/shm/` prefix | Ephemeral | High |
| `$TMPDIR/` prefix | Ephemeral | High |
| `.tmp`/`.temp` in path component | Ephemeral | Medium |
| `test_`/`test-` path component | Ephemeral | Medium |
| `_test`/`-test` path component | Ephemeral | Medium |

### 2. Test Harness Context

| Signal | Classification | Confidence |
|--------|---------------|------------|
| `CARGO_TARGET_DIR` set | Test context | Medium |
| `RUST_TEST_THREADS` set | Test context | High |
| `AM_TEST_MODE=true` | Test context | High |
| Parent process is `cargo-test` | Test context | High |
| `CI=true` environment | CI context | High |

### 3. Synthetic Agent Sessions

| Signal | Classification | Confidence |
|--------|---------------|------------|
| `NTM_` environment prefix | NTM swarm | High |
| Agent name matches `^Test[A-Z]` | Test agent | Medium |
| Agent name matches `^Repro[A-Z]` | Repro agent | Medium |
| Session started by `am e2e run` | E2E test | High |

### 4. Repro/Debug Directories

| Pattern | Classification | Confidence |
|---------|---------------|------------|
| `/repro-*/` in path | Repro run | High |
| `/debug-*/` in path | Debug session | Medium |
| `/incident-*/` in path | Incident investigation | Medium |
| `repro_` prefix in project slug | Repro project | Medium |

---

## Default Behaviors by Classification

### Non-Ephemeral (Production)

| Operation | Behavior |
|-----------|----------|
| Storage root | Use configured `STORAGE_ROOT` or XDG default |
| SQLite path | Use configured `DATABASE_URL` or storage-root-relative |
| Archive writes | Commit to shared Git archive |
| Agent registration | Persisted to shared database |
| Message delivery | Full durability guarantees |

### Ephemeral (Default Safe Mode)

| Operation | Behavior |
|-----------|----------|
| Storage root | **Auto-reroute** to isolated temp location |
| SQLite path | Use ephemeral-specific in-memory or temp DB |
| Archive writes | Isolated Git worktree or no-op |
| Agent registration | Session-scoped only |
| Message delivery | Best-effort, no durability guarantee |

### Ephemeral with `--allow-shared-mailbox`

| Operation | Behavior |
|-----------|----------|
| Storage root | Use configured shared root (explicit consent) |
| SQLite path | Use shared database (explicit consent) |
| Warning | Emit prominent warning on startup |
| Audit log | Record ephemeral-context access in evidence ledger |

---

## Classification Priority

When multiple signals conflict, apply this priority (highest to lowest):

1. **Explicit environment override**: `AM_EPHEMERAL_MODE=force|deny`
2. **Explicit CLI flag**: `--ephemeral` or `--no-ephemeral`
3. **Test harness context** (RUST_TEST_THREADS, CI, cargo-test parent)
4. **Path-based classification** (tmp, var/tmp, dev/shm)
5. **Pattern-based classification** (test_, repro_, .tmp)
6. **Default**: Non-ephemeral (production mode)

---

## Isolated Storage Root Algorithm

When ephemeral mode is detected and no explicit `--allow-shared-mailbox`:

```
base = $AM_EPHEMERAL_ROOT or $TMPDIR or "/tmp"
suffix = hash(project_root + session_id + timestamp)
isolated_root = "{base}/.am-ephemeral/{suffix}"
```

Requirements:
- **Deterministic**: Same inputs produce same isolated root
- **Discoverable**: `am doctor list-ephemeral-roots` enumerates active roots
- **Self-cleaning**: Roots older than 24 hours with no active locks are garbage-collected

---

## Diagnostics

### Startup Messages

**Ephemeral detected, auto-isolated:**
```
[info] Ephemeral context detected (project in /tmp).
[info] Using isolated storage root: /tmp/.am-ephemeral/a1b2c3d4
[info] To use the shared mailbox, set AM_EPHEMERAL_MODE=deny or pass --allow-shared-mailbox
```

**Ephemeral with explicit shared access:**
```
[warn] Ephemeral context detected (project in /tmp).
[warn] --allow-shared-mailbox granted; using shared mailbox at /home/ubuntu/.mcp_agent_mail_git_mailbox_repo
[warn] Changes will affect the production archive.
```

### Doctor Check Output

```
{
  "check": "ephemeral_context",
  "status": "ok",
  "detail": "Ephemeral context detected; storage isolated to /tmp/.am-ephemeral/a1b2c3d4",
  "classification": {
    "signals": ["path_contains_tmp", "rust_test_threads_set"],
    "mode": "auto_isolated",
    "shared_access": false
  }
}
```

---

## Configuration Variables

| Variable | Default | Description |
|----------|---------|-------------|
| `AM_EPHEMERAL_MODE` | `auto` | `auto` (detect), `force` (always isolate), `deny` (never isolate) |
| `AM_EPHEMERAL_ROOT` | `$TMPDIR` | Base directory for isolated ephemeral roots |
| `ALLOW_EPHEMERAL_PROJECTS_IN_DEFAULT_STORAGE` | `false` | Legacy flag; equivalent to `AM_EPHEMERAL_MODE=deny` |

---

## Implementation Requirements

### Core (`mcp-agent-mail-core`)

```rust
/// Classification of a project's ephemeral status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EphemeralClass {
    /// Definitely ephemeral (high-confidence signal).
    Ephemeral,
    /// Probably ephemeral (medium-confidence signals).
    LikelyEphemeral,
    /// No ephemeral signals detected.
    Production,
}

/// Signals that contributed to ephemeral classification.
#[derive(Debug, Clone)]
pub struct EphemeralSignals {
    pub path_tmp: bool,
    pub path_var_tmp: bool,
    pub path_dev_shm: bool,
    pub path_tmpdir: bool,
    pub path_contains_test: bool,
    pub path_contains_repro: bool,
    pub env_rust_test: bool,
    pub env_ci: bool,
    pub env_ntm: bool,
    pub parent_cargo_test: bool,
}

/// Classify a project root as ephemeral or production.
pub fn classify_ephemeral(
    project_root: &Path,
    env: &impl Fn(&str) -> Option<String>,
) -> (EphemeralClass, EphemeralSignals);

/// Resolve the effective storage root for a project.
pub fn resolve_storage_root(
    config: &Config,
    project_root: &Path,
    ephemeral_class: EphemeralClass,
) -> PathBuf;
```

### CLI

- `am doctor check` includes ephemeral context in output
- `am doctor list-ephemeral-roots` enumerates isolated roots
- `am doctor clean-ephemeral-roots [--older-than=24h]` garbage-collects stale roots

### Server

- Startup logging includes ephemeral classification
- TUI displays isolation status in SystemHealth screen

---

## Testing Requirements

1. **Unit tests**: Every pattern/signal combination produces correct classification
2. **Integration tests**: Auto-isolation creates expected directory structure
3. **E2E tests**: Ephemeral runs do not modify shared mailbox
4. **Cleanup tests**: Garbage collection removes only expired roots

---

## Migration Path

1. **Phase 1** (this bead): Define classification and policy (no runtime changes)
2. **Phase 2** (br-97gc6.5.2.2.2): Implement auto-reroute for detected ephemeral contexts
3. **Phase 3** (br-97gc6.5.2.2.3): Add `--allow-shared-mailbox` opt-in with warning
4. **Phase 4** (br-97gc6.5.2.2.6): Implement deterministic isolated roots and cleanup

---

## Changelog

- **2026-04-02**: Initial draft per bead br-97gc6.5.2.2.1
