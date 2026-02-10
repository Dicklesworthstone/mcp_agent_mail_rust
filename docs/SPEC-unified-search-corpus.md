# Spec: Unified Search Corpus Schema + FTS Migration Strategy

**Bead:** br-3vwi.2.1  
**Track:** br-3vwi.2  
**Date:** 2026-02-10  
**Status:** Draft

## Scope

Define the DB-level "search corpus" for Agent Mail:

- What entities are searchable (messages, threads, agents, projects)
- What fields are indexed (and how)
- How FTS5 tables are maintained (triggers/backfills) with bounded write amplification
- How migrations stay deterministic and idempotent

This spec is the controlling artifact for:

- `br-3vwi.2.2` (global query planner: facets, ranking, stable pagination, explainability)
- `br-3vwi.2.3` (server API integration + telemetry/perf harness)
- `br-3vwi.2.5` (search quality benchmark corpus + relevance tuning harness)

And it informs:

- `br-3vwi.2.4` (permission-aware visibility + redaction guardrails)

## Current State (2026-02-10)

### Messages

- `messages(project_id, sender_id, thread_id, subject, body_md, importance, ack_required, created_ts, ...)`
- `fts_messages(message_id, subject, body, tokenize='porter unicode61 remove_diacritics 2', prefix='2,3')`
- Triggers: `messages_ai/ad/au` keep `fts_messages` in sync.

Refs:
- `crates/mcp-agent-mail-db/src/schema.rs`
- `crates/mcp-agent-mail-db/src/queries.rs` (`sanitize_fts_query`, `search_messages`)

### Message Search API

- DB query: `queries::search_messages(project_id, query, limit)`
  - Uses `fts_messages MATCH ?`
  - Orders by `bm25(fts_messages, 10.0, 1.0)` then `m.id`
  - On FTS error, falls back to LIKE on extracted terms
- MCP tool: `crates/mcp-agent-mail-tools/src/search.rs` resolves `project_key -> project_id` and calls DB search.
- TUI currently has its own search sanitization/fallback path (needs convergence later).

### Product Search

- MCP tool `search_messages_product` currently:
  - Lists projects linked to product
  - Loops `queries::search_messages` per project
  - Truncates results
- CLI already does a single query across projects (no per-project loop).

Problem: per-project loops create N+1 query cost and produce non-global ranking/truncation bias.

## Goals

- Provide a unified corpus definition across:
  - Messages
  - Threads
  - Agents
  - Projects
- Support cross-project/product queries without per-project loops.
- Support facets/filters needed by the global planner:
  - product, project, agent, thread, direction (inbox/outbox), importance, ack_required, time ranges
- Keep behavior deterministic:
  - stable ordering
  - stable pagination anchors
  - safe query sanitization and fail-closed behavior
- Keep write amplification bounded, explicit, and testable.
- Maintain SQLite portability (FTS5).

## Non-Goals

- Implement the full planner/query language (`br-3vwi.2.2`).
- Implement permission/redaction enforcement (`br-3vwi.2.4`) beyond schema hooks.
- External search engines (Elastic/Meilisearch/etc).

## Search Corpus Model

A *search document* is a logical unit with:

- `doc_kind`: `message | thread | agent | project`
- `doc_pk`: stable identity (integer id for tables; stable text id for threads)
- `project_id`: optional depending on `doc_kind`
- `created_ts` / `updated_ts`: for stable pagination
- `title` / `body`: indexed text
- `facets`: stored or derivable metadata

Implementation may be either:

1. Multiple entity-specific FTS tables (recommended v1 to avoid re-indexing messages).
2. A single consolidated FTS table (`fts_corpus`) (v2 option if planner complexity/perf demands).

## Recommended v1 Implementation (No Double-Indexing Messages)

### Messages (keep `fts_messages`)

Keep `fts_messages` as the message portion of the corpus.

Planned enhancements (separate bead; not required for `br-3vwi.2.1`):

- Consider extending the indexed text to include `thread_id` and sender/recipient names to improve discoverability.
- Narrow `messages_au` to `AFTER UPDATE OF subject, body_md` to avoid needless churn on non-indexed updates.

### Agents (add `fts_agents`)

Add `fts_agents` that indexes stable agent identity fields:

- Indexed: `name`, `task_description`
- Unindexed metadata: `agent_id`, `project_id`, `program`, `model`

CRITICAL: `agents.last_active_ts` updates frequently; triggers MUST NOT fire on that column.

Proposed schema:

```sql
CREATE VIRTUAL TABLE IF NOT EXISTS fts_agents USING fts5(
  agent_id UNINDEXED,
  project_id UNINDEXED,
  name,
  task_description,
  program UNINDEXED,
  model UNINDEXED,
  tokenize='porter unicode61 remove_diacritics 2',
  prefix='2,3'
);
```

Triggers:

- `agents_ai` AFTER INSERT ON agents
- `agents_ad` AFTER DELETE ON agents
- `agents_au` AFTER UPDATE OF name, task_description, program, model ON agents

Backfill (migration):

```sql
INSERT OR REPLACE INTO fts_agents(agent_id, project_id, name, task_description, program, model)
SELECT id, project_id, name, task_description, program, model FROM agents;
```

### Projects (add `fts_projects`)

Add `fts_projects` that indexes:

- Indexed: `slug`, `human_key`
- Unindexed metadata: `project_id`

Schema:

```sql
CREATE VIRTUAL TABLE IF NOT EXISTS fts_projects USING fts5(
  project_id UNINDEXED,
  slug,
  human_key,
  tokenize='porter unicode61 remove_diacritics 2',
  prefix='2,3'
);
```

Triggers:

- `projects_ai` AFTER INSERT ON projects
- `projects_ad` AFTER DELETE ON projects
- `projects_au` AFTER UPDATE OF slug, human_key ON projects

Backfill:

```sql
INSERT OR REPLACE INTO fts_projects(project_id, slug, human_key)
SELECT id, slug, human_key FROM projects;
```

### Threads (derived v1; optional materialization later)

Threads are currently implicit (`messages.thread_id`), not a first-class table.

v1 approach:

- Thread-level search is derived from message search results:
  - Run FTS over messages
  - Group by `thread_id`
  - Rank a thread by its best (minimum) `bm25` among matched messages

Materialization option (future):

- Add a `threads` rollup table and optional `fts_threads` if thread search becomes a hot path.

## Cross-Project / Product Search

Immediate fix (should land alongside this spec):

- Implement a single DB query that searches messages across all projects linked to a product using
  `JOIN product_project_links` (or `IN (SELECT ...)`), returning globally ranked results.

This removes per-project loops and makes ordering correct.

## Query Determinism & Explainability

- Deterministic ordering: `ORDER BY score ASC, id ASC` (where `score` is `bm25(...)`).
- Stable pagination: planner should use `(score, id)` cursor components, not just `id`.
- Explainability (planner output; `br-3vwi.2.2`): optionally return `score`, matched terms, and entity kind.

## Safety / Sanitization

- Canonical sanitizer: `mcp_agent_mail_db::queries::sanitize_fts_query` (ports Python behavior).
- On FTS syntax errors, fallback to LIKE over extracted terms.
- Future: converge TUI sanitization/fallback to the same DB helper to avoid behavioral drift.

## Migration Strategy (Idempotent)

Migrations must be single SQLite statements (see `crates/mcp-agent-mail-db/src/schema.rs`).

For new FTS tables:

1. `CREATE VIRTUAL TABLE IF NOT EXISTS ...`
2. `CREATE TRIGGER IF NOT EXISTS ... END`
3. `INSERT OR REPLACE INTO ... SELECT ...` (backfill)
4. Optional: `ANALYZE` after bulk backfills

Reruns must be safe:

- Use `IF NOT EXISTS` on creates.
- Use `INSERT OR REPLACE` for backfills.
- Use `AFTER UPDATE OF ...` triggers to avoid write amplification on non-indexed updates.

## Write Amplification Budgets

Baseline today:

- Each message insert causes one additional FTS write via `messages_ai`.

v1 target:

- Agent/project FTS maintenance is negligible compared to message traffic.
- Agent triggers must *not* fire on `last_active_ts` churn.

Hard constraint (v1):

- No per-recipient FTS rows for direction facets; direction is derived via joins against `message_recipients`.

## Benchmarks (Inputs for `br-3vwi.2.5`)

Measure on representative dataset sizes:

- Ingest throughput: messages/sec with FTS triggers enabled.
- Query latency (p50/p95):
  - Project message search
  - Product message search (N projects)
  - Thread-grouped search
- Storage overhead:
  - Total DB size
  - FTS segment size deltas per entity

## Tests

- Unit tests:
  - `sanitize_fts_query` edge cases (already present)
  - LIKE fallback term extraction (already present)
- Integration tests:
  - Product-wide search returns globally ranked results across linked projects
  - FTS triggers keep virtual tables consistent after insert/update/delete
  - Migration/backfill idempotency (run migrations twice)
- Regression (future):
  - Enforce “no N+1” in product search via query-count telemetry

## Implementation Pointers

- DB schema/migrations: `crates/mcp-agent-mail-db/src/schema.rs`
- DB queries: `crates/mcp-agent-mail-db/src/queries.rs`
- MCP tools:
  - Project search: `crates/mcp-agent-mail-tools/src/search.rs`
  - Product search: `crates/mcp-agent-mail-tools/src/products.rs`
- CLI reference implementation (single-query product search):
  - `crates/mcp-agent-mail-cli/src/lib.rs`
- TUI search path (needs convergence later):
  - `crates/mcp-agent-mail-server/src/tui_screens/messages.rs`

