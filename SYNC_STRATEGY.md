# Consistency Contract

> Authoritative reference for write ordering, crash safety, and reconciliation
> in MCP Agent Mail. Updated 2026-02-08 as part of br-15dv.6.7.

## Source of Truth

- **Primary**: SQLite database (transactional, immediate consistency).
- **Secondary**: Git archive (human-auditable, append-only, eventually consistent).
- **Rationale**: All tool calls return success/failure based on DB state. The
  archive is a best-effort mirror that can lag under load or disk pressure. The
  DB is always rebuildable from archive if corruption occurs, and the archive
  is always reconstructible from DB if files are lost.

### Why DB-First (Not Archive-First)

The original design aspired to archive-first writes, but the implementation
evolved to DB-first for these reasons:

1. **Atomicity**: SQLite transactions give all-or-nothing semantics. Git commits
   are multi-step (stage → tree → commit) and cannot be rolled back atomically.
2. **Performance**: DB writes are synchronous and sub-millisecond. Archive writes
   go through a Write-Behind Queue (WBQ) + Commit Coalescer, adding 50-200ms
   latency. Blocking on archive would make every tool call 100x slower.
3. **Concurrency**: At 1000+ agents, git index.lock contention makes synchronous
   archive writes impractical. The coalescer batches commits across agents.
4. **Failure isolation**: Archive failures (disk full, git corruption, lock storms)
   must not prevent agents from communicating. DB remains available.

## Write Semantics Per Tool

### Strong Consistency (DB-only, no archive write)

These tools modify only DB state and return success iff the DB write commits:

| Tool | DB Operation | Idempotent |
|------|-------------|------------|
| `mark_message_read` | UPDATE `read_ts` via COALESCE | Yes |
| `acknowledge_message` | UPDATE `read_ts` + `ack_ts` via COALESCE | Yes |
| `release_file_reservations` | UPDATE `released_ts` WHERE NULL | Yes |
| `renew_file_reservations` | UPDATE `expires_ts` | Yes |
| `set_contact_policy` | UPDATE agent policy | Yes |

### Eventual Consistency (DB + deferred archive)

These tools write to DB first, then enqueue archive writes to the WBQ:

| Tool | DB Write | Archive Write | Idempotent |
|------|----------|---------------|------------|
| `ensure_project` | INSERT/SELECT project | ensure_archive dir | Yes |
| `register_agent` | INSERT/UPDATE agent | WBQ: profile.json | Yes |
| `create_agent_identity` | INSERT agent (reject dup) | WBQ: profile.json | No |
| `send_message` | INSERT message + recipients | WBQ: canonical + inbox/outbox | No |
| `reply_message` | INSERT message + recipients | WBQ: canonical + inbox/outbox | No |
| `file_reservation_paths` | INSERT per-path reservations | WBQ: JSON artifacts | No |
| `force_release_file_reservation` | UPDATE released_ts + INSERT notify | N/A | Partial |
| `request_contact` | INSERT/UPDATE contact link | WBQ: intro message | Partial |
| `respond_contact` | UPDATE contact link | N/A | Yes |
| `install_precommit_guard` | N/A (filesystem only) | Write hook script | Yes |

### Write Ordering Guarantee

For all eventual-consistency tools:

```
1. DB transaction commits (BEGIN IMMEDIATE → COMMIT)
2. Tool returns success to caller
3. Archive write enqueued to WBQ (non-blocking)
4. WBQ drain thread writes files to disk (atomic: temp + rename)
5. Commit Coalescer batches git commits (50ms window, 4 shards)
```

**Invariant**: A tool NEVER returns success if the DB write failed. Archive
write failure does NOT cause tool failure.

## Failure Modes and Recovery

### DB write fails

- Tool returns error to caller.
- No archive write attempted.
- Caller can safely retry.
- No orphaned state.

### DB succeeds, archive write skipped

Causes: disk pressure (Critical level), WBQ channel disconnected, WBQ full.

- Tool returns success (DB is source of truth).
- A reconciliation marker is recorded in `needs_reindex` metrics.
- On next server restart, the consistency probe detects any divergence.
- Manual recovery: `am reconcile` CLI command rebuilds archive from DB.

### DB succeeds, archive files written, git commit fails

Causes: index.lock contention after retries, git corruption.

- Files are on disk (atomic write via temp+rename).
- Commit coalescer retries with exponential backoff (5-7 attempts).
- Last resort: stale index.lock removed after 60s.
- If all retries fail: files discoverable on disk, next commit picks them up.
- Metric: `commit_errors_total` incremented.

### Server crash mid-write

- **DB**: SQLite WAL journal ensures either full commit or full rollback.
- **Archive files**: Atomic write (temp+rename) means either complete file or no file.
- **Git commits**: Partial staging is cleaned up by git on next operation.
- On restart: integrity probe runs PRAGMA quick_check; archive lock healing
  removes stale `.archive.lock` and `.git/index.lock` files.

### SQLite corruption

- Startup probe detects via `PRAGMA quick_check`.
- Server refuses to start with remediation guidance.
- Recovery: VACUUM INTO to salvage data, then rebuild from archive.

## Idempotency Contract

| Category | Guarantee | Mechanism |
|----------|-----------|-----------|
| Read/ack operations | Fully idempotent | COALESCE(existing, new) prevents overwrite |
| Release/renew | Fully idempotent | WHERE released_ts IS NULL / expires_ts check |
| register_agent | Idempotent (upsert) | SELECT-then-INSERT/UPDATE on (project, name) |
| ensure_project | Idempotent | UNIQUE(slug) + retry-on-conflict |
| send_message | NOT idempotent | Each call creates new message with fresh ID |
| reply_message | NOT idempotent | Each call creates new message with fresh ID |
| file_reservation_paths | NOT idempotent | Each call creates new reservations |
| create_agent_identity | NOT idempotent | Rejects duplicate names |

For non-idempotent tools, callers must implement their own deduplication
(e.g., checking if a message with the same content was recently sent).

## Concurrency

- **DB**: Pool with configurable min/max connections (default 25/100).
  `BEGIN IMMEDIATE` serializes writes; reads can proceed concurrently (WAL mode).
- **Archive locks**: Per-project `.archive.lock` for file writes.
  Per-project `.commit.lock` for git operations.
- **WBQ**: Single mpsc channel (capacity 8192) with one drain thread.
- **Commit coalescer**: 4 shards, each with independent worker thread.
  Repo root hashed to shard for locality.

## Metrics for Monitoring

| Metric | What it tells you |
|--------|-------------------|
| `wbq_depth` | Current WBQ backlog; high = archive lagging DB |
| `wbq_errors_total` | Archive write failures; rising = investigate disk/git |
| `commit_errors_total` | Git commit failures; rising = index.lock contention |
| `commit_sync_fallbacks_total` | Coalescer worker died; synchronous fallback used |
| `needs_reindex_total` | DB rows without corresponding archive files |
| `pool_acquire_latency_us` (p95) | DB contention; Yellow > 10ms, Red > 50ms |

## Reconciliation

### Startup Consistency Probe

On server startup (after integrity check), a lightweight probe samples recent
DB messages and verifies corresponding archive files exist. This catches
persistent archive-DB divergence.

- Checks last N messages (configurable, default 100).
- For each, verifies canonical archive path exists on disk.
- Reports count of missing archive files as `needs_reindex_total`.
- Does NOT block startup; logs warnings for operator visibility.

### Manual Reconciliation

```bash
# Check archive-DB consistency
am reconcile --check

# Rebuild missing archive files from DB
am reconcile --rebuild

# Rebuild DB index from archive files
am reconcile --reindex
```
