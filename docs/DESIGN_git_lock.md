# Design: Per-Repo Git Lock Hierarchy — br-8ujfs.2.1 (B1)

**Epic:** br-8ujfs (Git 2.51.0 concurrency hardening)
**Bead:** br-8ujfs.2.1 (B1)
**Author:** RubyKnoll
**Date:** 2026-04-19
**Status:** RFC → will be merged to main as the source of truth for
Track B (B2..B6) implementation.

## 1. Purpose

Define the lock hierarchy that serializes in-process `git`
shell-outs and coordinates with external git processes, so the
2.51.0 index-race bug cannot cause us to corrupt a user's repo.

This is the foundational design note for Track B. Every code
decision in B2/B3/B4/B5/B6 derives from this document; future
readers who want to understand "why is this lock here?" should
land here first.

## 2. Goals

1. Zero races between **our own** threads calling git on the same
   repo.
2. Bounded-contention cooperation with **peer processes** (a second
   mcp-agent-mail instance, a wrapper-script git in another shell)
   via OS-level locks.
3. Fail-forward under pathological conditions: never deadlock,
   never hang indefinitely.
4. No regression for the happy path: uncontended git calls must see
   < 1ms additional latency.

## 3. Non-goals

- **External unwrapped git** (the user's IDE running `git commit`
  without any coordination) is NOT protected by this lock. That
  case is mitigated only by:
  - Layer 1: running a non-broken git version (A5's AM_GIT_BINARY).
  - Layer 5: retry on SIGSEGV (Track E).
  Documenting this gap explicitly prevents false confidence.
- **Cross-host coordination** (two servers on different machines
  touching the same repo over NFS). Out of scope; the bug only
  matters when writers and readers share the same mmap.
- **Global process mutex** (one-big-lock style). Per-repo lock is
  both necessary (different repos must run in parallel) and
  sufficient.

## 4. Lock hierarchy

### 4.1 Layer A — In-process mutex (`GitRepoLocks`)

- Pure in-memory `Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>` keyed by
  the canonical repo path.
- Outer `Mutex` guards map insertion; inner `Arc<Mutex<()>>` is
  what callers actually lock on.
- Map is monotonically growing. No eviction (bounded by project
  count; practical cap ~100s).
- Canonicalization: `std::fs::canonicalize(repo)` on first insertion.
  Falls through unlocked if the path does not exist (caller's
  responsibility to ensure existence).
- Latency: uncontended acquire is nanoseconds.

### 4.2 Layer B — OS flock (`RepoFlock`)

- `fcntl(F_SETLK)` on `<admin_dir>/am.git-serialize.lock` via the
  `fs2` crate (portable wrapper that also maps correctly to Windows
  `LockFileEx`).
- `admin_dir` resolved via `git2::Repository::open_ext`'s reported
  admin directory:
  - Normal repo: `<repo>/.git/`
  - Bare repo: `<repo>/` (repo IS the admin dir)
  - Linked worktree: `<main>/.git/worktrees/<name>/`
  - `GIT_DIR` env: `$GIT_DIR/` (honored)
- Sentinel file is never unlinked — unlinking while a peer holds
  the lock breaks semantics on some filesystems (POSIX allows it;
  NFS may not).
- Sentinel file lives alongside `.git/config` etc. and is
  gitignore-irrelevant because `.git/` isn't tracked.

### 4.3 Acquisition order (strict)

1. Canonicalize the repo path.
2. Acquire `GitRepoLocks::global().lock_for(&canonical)` (the
   inner `Arc<Mutex<()>>`).
3. Acquire `RepoFlock::acquire(&canonical)` (OS flock).
4. Run the git operation.
5. Drop RepoFlock (unlock syscall).
6. Drop mutex guard (in-process release).

Opposite order on release. This order is fixed because:
- Mutex-then-flock avoids the pathological case where two threads
  in the same process both wait on flock while one of them could
  have been parked on the mutex.
- Flock-then-unmutex on release means a peer process that was
  waiting on flock runs immediately, without fighting in-process
  threads.

### 4.4 Why NOT `.git/index.lock` as the flock sentinel

`.git/index.lock` is git's own sentinel for its index write
critical section. Grabbing it would:
- Race against git's own lock protocol (git handles it via
  `unlink` + atomic create, assuming it owns the file).
- Confuse every other git tool that checks for its presence.
- Not actually fix the 2.51.0 bug: the bug is in the READ path's
  mmap, not in the write lock. The writer already takes
  index.lock correctly; the problem is that truncate happens
  during the rewrite.

A dedicated mcp-agent-mail-only sentinel is correct.

## 5. Scope of each layer

| Layer | Coordinates within | Does not coordinate with |
|---|---|---|
| GitRepoLocks (mutex) | Threads of one process | Other processes |
| RepoFlock (fcntl flock) | All processes that take the SAME sentinel | Processes that don't take the sentinel |

The `RepoFlock` gap is the fundamental reason Layer 1 (AM_GIT_BINARY,
downgrade) is the first line of defense. A user's `git commit` from
their IDE doesn't take our sentinel and therefore races against our
locked-down shell-outs.

## 6. Edge cases and their handling

### 6.1 Reentrancy

**Policy:** Strict refusal. Same thread + same canonical repo path
inside an existing `run_git_locked` call panics:

```
panic: run_git_locked reentrant call on <path> from thread <id>.
This would deadlock. Stack trace captured in tracing event
git_reentrancy_panic.
```

Implementation: `thread_local RefCell<HashSet<PathBuf>>` tracking
held locks per thread.

Rationale: reentrancy on an advisory fcntl lock has undefined
semantics (POSIX says the same process holding a lock can't block
itself, but this makes reasoning about correctness hard). We have
no legitimate reason to nest. Panic surfaces bugs immediately; hiding
behind a "reentrant-ok" flag would paper over real defects.

### 6.2 Timeouts

- **In-process mutex**: no timeout. Contention is bounded by the
  innermost git op duration (< 30s for any reasonable op). If we
  time out here something is catastrophically wrong.
- **OS flock**: 60s ceiling.
  - Phase 1: `try_lock_exclusive` (non-blocking). On success,
    immediate proceed.
  - Phase 2: on WouldBlock, log INFO `flock_waiting`; spawn watchdog
    that logs WARN every 5s with elapsed time.
  - Phase 3: at 60s, abort with `io::Error(TimedOut)`. The abort
    message includes the sentinel path and elapsed time for
    operator triage.
- **Child process (the git op itself)**: 120s wall-clock ceiling
  inside `run_git_locked` (B4). `git log --all` on a huge repo
  should complete in < 30s; 120s is generous. Killing with SIGKILL
  on timeout, logging ERROR.

Total worst-case before a caller sees an error: 60s flock + 120s
exec = 180s. Documented.

### 6.3 Bare repositories

- `workdir()` is None.
- `admin_dir` IS the repo root.
- Sentinel: `<repo>/am.git-serialize.lock` (not under `.git/`).
- Otherwise identical to normal repos.

### 6.4 Linked worktrees

- `.git` in the worktree is a FILE containing
  `gitdir: /path/to/main/.git/worktrees/<name>`.
- libgit2's `Repository::open` follows this automatically.
- Sentinel lives with the WORKTREE's admin data:
  `<main>/.git/worktrees/<name>/am.git-serialize.lock`, NOT
  `<main>/.git/am.git-serialize.lock`. This prevents worktree
  operations from blocking each other inappropriately.

### 6.5 GIT_DIR environment variable

- Legal and occasionally used (points `.git` at a separate dir).
- `sentinel_path` honors `GIT_DIR` if set (libgit2 already does
  via `open_ext`).

### 6.6 Symlinked `.git`

- Some tools (git-new-workdir historical, some CI) symlink `.git`.
- Sentinel lands on the REAL admin dir (canonicalize before writing).

### 6.7 Read-only `.git` directory

- Can happen on read-only checkouts or permission-restricted
  filesystems.
- Sentinel open fails with EACCES/EPERM.
- Decision: WARN once per repo per process, return a "phantom"
  RepoFlock that holds nothing. B4 treats this as flock-skipped;
  mutex still applies. Logged as
  `flock_readonly_fallback`. Sticky once detected (no retries).

### 6.8 Network filesystems (NFS, SMB)

- `fcntl` flock on NFS works but can leak stale locks if the
  server reboots. On SMB, flock is best-effort.
- Decision: DOCUMENT the caveat. Emit `flock_network_filesystem_
  detected` WARN once per session per repo (detected via `statfs`
  fs type on Linux; best-effort on other OSes).
- Does NOT disable flock on NFS — advisory locks still help more
  than they hurt.

### 6.9 File handle lifecycle

- Sentinel opened with `OpenOptions::new().create(true).read(true).
  write(true)`.
- Handle stored in `RepoFlock`; `Drop` impl issues `LOCK_UN` via
  fs2 before closing.
- Process crash: kernel auto-releases fcntl locks at close time.
  No leaked locks across restarts.
- Sentinel file itself persists forever (no unlink) — harmless;
  size 0 bytes.

### 6.10 Test isolation

- Each test uses `tempfile::tempdir()` for a unique repo path.
- `GitRepoLocks::global()` (OnceCell) persists across tests in the
  same `cargo test` process — that's fine; unique paths map to
  unique mutexes.
- Metrics counters are shared. Use `mcp_agent_mail_core::metrics::
  test_reset()` in test setup OR assert on deltas (snapshot +
  invoke + snapshot + compare).

## 7. Rejected alternatives

### 7.1 One big mutex

"Put everything that calls git behind one global `Mutex<()>`."

Rejected: destroys cross-repo parallelism, which the mailbox
archive depends on (CommitCoalescer workers on different projects
must run concurrently). Also doesn't solve the external-git problem
(unwrapped git still races).

### 7.2 libgit2 for everything

"Migrate 100% of shell-outs to libgit2."

Rejected as the ONLY strategy because:
- Some ops don't have clean libgit2 equivalents (streaming
  `ls-files` with kill-on-match, per cleanup.rs:554).
- Some ops should keep CLI semantics (user-visible commit in
  cli/src/lib.rs:13639 — must respect `.gitattributes`, clean
  filters, etc., matching the user's expectation).
- Guard runs inside the user's hook; libgit2 isn't the right
  layer there.
- See Track C (MIGRATE) + Track D (WRAP) as complementary
  strategies. Track B provides the infrastructure for Track D.

### 7.3 Hook into git directly (LD_PRELOAD, ptrace)

Rejected: fragile, platform-specific, doesn't survive git upgrades,
and adds substantial complexity for marginal benefit. If the user
wants to use a specific git binary we give them `AM_GIT_BINARY`
(A5) which is simpler and more portable.

### 7.4 Shared-memory coordination between processes

Rejected: POSIX shared memory works but is operationally fragile
(cleanup, permissions, security). fcntl flock is the canonical
cross-process coordination primitive for file-backed state and
works on every platform we target.

## 8. Sequence diagrams

### 8.1 Typical `run_git_locked` call

```
Caller        GitRepoLocks     RepoFlock       git process
  |                |                |                 |
  |-- canonicalize|                |                 |
  |---- lock_for ->|                |                 |
  |<-- Arc<Mutex>  |                |                 |
  |-- mutex.lock  ->|                |                 |
  |<-- guard       |                |                 |
  |-- acquire ----------------------->|               |
  |                |   (fcntl F_SETLK LOCK_EX)       |
  |<-- RepoFlock   |                |                 |
  |-- spawn ------------------------------------------->|
  |                                                    |
  |                                  (git runs)        |
  |                                                    |
  |<-- Output     <-------------------------------------|
  |-- drop RepoFlock                 |                  |
  |                (LOCK_UN)         |                  |
  |-- drop guard   |                |                  |
```

### 8.2 Contention (peer process waiting)

```
Process A (holds mutex + flock)        Process B
  |                                       |
  |-- acquire flock (OK)                  |-- acquire flock (WouldBlock)
  |                                       |-- INFO flock_waiting
  |                                       |-- sleep, retry every 5s
  |                                       |    -> WARN after 5s
  |                                       |    -> WARN after 10s
  |-- drop flock                          |
  |                                       |<- acquire flock (OK)
```

### 8.3 60s timeout abort

```
Process A (holds flock, stuck)         Process B
  |                                      |-- acquire flock
  |                                      |-- WouldBlock
  |                                      |-- sleep ~60s, 11x WARN
  |                                      |-- abort, io::Error(TimedOut)
  |                                      |-- caller sees clean error
```

## 9. Correctness arguments

### 9.1 No deadlock

- Layer ordering is strict (mutex → flock, flock → mutex on release).
- Reentrancy panics before it can deadlock (detected via thread_local
  HELD set).
- No nested lock-of-different-things; only the same repo's two
  layers.

### 9.2 No livelock

- Each acquire makes progress: either `try_lock` succeeds (happy
  path) or blocks on `LOCK_EX` which the kernel fairly queues
  (fcntl is FIFO on Linux; BSD and Darwin similar).

### 9.3 No silent lock skip

- Reentrancy: panics (not silent skip).
- Read-only sentinel: logged WARN and documented as degraded mode.
- Canonicalize failure: falls through unlocked with WARN.

## 10. Implementation roadmap for Track B

| Bead | Deliverable |
|---|---|
| B2 | `GitRepoLocks` with canonicalization + per-test tests (Section 4.1) |
| B3 | `RepoFlock` with sentinel resolution, timeouts, edge cases 6.3-6.8 |
| B4 | `run_git_locked` / `GitCmd` builder combining B2 + B3 + AM_GIT_BINARY resolution + watchdog + reentrancy guard (Sections 4.3, 6.1, 6.2, 6.9) |
| B5 | `scripts/git-with-amlock.sh` wrapper for external tools |
| B6 | README + AGENTS.md documentation for opt-in external coordination |
| B7 | `am guard refresh-hooks` to propagate hook updates |

## 11. Review

Design note reviewed by: (to be populated via agent-mail thread)

Before B2/B3/B4 start coding, the review sign-off must land in:
- This section of the design note; OR
- A closed comment on `br-8ujfs.2.1` naming at least one reviewer.

Absence of review: STOP and request one.

## 12. Changelog

- **2026-04-19 v1** (RubyKnoll, br-8ujfs.2.1): initial draft,
  covers edge cases 6.1-6.10 and scope clarifications from
  comment-revision v3.
