# Existing MCP Agent Mail Structure (Legacy Python)

> **THE SPEC** - Complete behavioral specification extracted from Python codebase.
> After reading this, you should NOT need to consult legacy code.

---

## Overview

- **Legacy codebase:** `/data/projects/mcp_agent_mail_rust/legacy_python_mcp_agent_mail_code/mcp_agent_mail`
- **Total size:** ~25,000 LOC Python
- **Primary modules:** `app.py` (10,261 LOC), `storage.py` (~3,600 LOC), `cli.py` (~5,000 LOC), `http.py` (~2,900 LOC), `db.py` (824 LOC), `config.py` (480 LOC), `guard.py` (~900 LOC), `models.py` (163 LOC), `llm.py` (257 LOC), `utils.py` (234 LOC)

## Entry Points

- `mcp_agent_mail.__main__.py` provides CLI entry safe under pytest
- `mcp_agent_mail.__init__.py` exposes `build_mcp_server()`
- `mcp_agent_mail.http.py` includes `main()` for HTTP server
- `mcp_agent_mail.cli.py` includes Typer CLI app and `if __name__ == "__main__"`

## CLI Commands (Typer)
Source: `src/mcp_agent_mail/cli.py`.

| Command | Args / Flags (from signatures) |
| --- | --- |
| `products ensure` | `product_key?`, `--name/-n` |
| `products link` | `product_key`, `project` |
| `products status` | `product_key` |
| `products search` | `product_key`, `query`, `--limit/-l` |
| `products inbox` | `product_key`, `agent`, `--limit/-l`, `--urgent-only/--all`, `--include-bodies/--no-bodies`, `--since-ts` |
| `products summarize-thread` | `product_key`, `thread_id`, `--per-thread-limit/-n`, `--no-llm` |
| `serve-http` | `--host`, `--port`, `--path` |
| `serve-stdio` | no flags |
| `lint` | no flags |
| `typecheck` | no flags |
| `share export` | `--output/-o`, `--interactive/-i`, `--project/-p` (repeat), `--inline-threshold`, `--detach-threshold`, `--scrub-preset`, `--chunk-threshold`, `--chunk-size`, `--dry-run/--no-dry-run`, `--zip/--no-zip`, `--signing-key`, `--signing-public-out`, `--age-recipient` (repeat) |
| `share update` | `bundle`, `--project/-p` (repeat), `--inline-threshold`, `--detach-threshold`, `--chunk-threshold`, `--chunk-size`, `--scrub-preset`, `--zip/--no-zip`, `--signing-key`, `--signing-public-out`, `--age-recipient` (repeat) |
| `share preview` | `bundle`, `--host`, `--port`, `--open-browser/--no-open-browser` |
| `share verify` | `bundle`, `--public-key` |
| `share decrypt` | `encrypted_path`, `--output/-o`, `--identity/-i`, `--passphrase/-p` |
| `share wizard` | no flags |
| `archive save` | `--project/-p` (repeat), `--scrub-preset`, `--label/-l` |
| `archive list` | `--limit/-n`, `--json` |
| `archive restore` | `archive_file`, `--force/-f`, `--dry-run` |
| `clear-and-reset-everything` | `--force/-f`, `--archive/--no-archive` |
| `migrate` | no flags |
| `list-projects` | `--include-agents`, `--json` |
| `guard install` | `project`, `repo`, `--prepush/--no-prepush` |
| `guard uninstall` | `repo` |
| `guard status` | `repo` |
| `guard check` | `--stdin-nul`, `--advisory`, `--repo` |
| `file_reservations list` | `project`, `--active-only/--no-active-only` |
| `file_reservations active` | `project`, `--limit` |
| `file_reservations soon` | `project`, `--minutes` |
| `acks pending` | `project`, `agent`, `--limit` |
| `acks remind` | `project`, `agent`, `--min-age-minutes`, `--limit` |
| `acks overdue` | `project`, `agent`, `--ttl-minutes`, `--limit` |
| `list-acks` | `--project`, `--agent`, `--limit` |
| `config set-port` | `port`, `--env-file` |
| `config show-port` | no flags |
| `amctl env` | `--path/-p`, `--agent/-a` |
| `am-run` | `slot`, `cmd...`, `--path/-p`, `--agent/-a`, `--ttl-seconds`, `--shared/--exclusive`, `--block-on-conflicts/--no-block-on-conflicts` |
| `projects mark-identity` | `project_path`, `--commit/--no-commit` |
| `projects discovery-init` | `project_path`, `--product/-P` |
| `projects adopt` | `source`, `target`, `--dry-run/--apply` |
| `mail status` | `project_path` |
| `docs insert-blurbs` | `--scan-dir/-d` (repeat), `--yes`, `--dry-run`, `--max-depth` |
| `doctor check` | `project?`, `--verbose/-v`, `--json` |
| `doctor repair` | `project?`, `--dry-run`, `--yes/-y`, `--backup-dir` |
| `doctor backups` | `--json` |
| `doctor restore` | `backup_path`, `--dry-run`, `--yes/-y` |

Structured CLI inventory artifact: `crates/mcp-agent-mail-conformance/tests/conformance/fixtures/cli/legacy_cli_inventory.json`.

## CLI Behavior Notes (Legacy Python)

### Share Commands (CLI → `share.py`)
- Default thresholds:
  - `INLINE_ATTACHMENT_THRESHOLD = 64 KiB`
  - `DETACH_ATTACHMENT_THRESHOLD = 25 MiB`
  - `DEFAULT_CHUNK_THRESHOLD = 20 MiB`
  - `DEFAULT_CHUNK_SIZE = 4 MiB`
- `share export`:
  - `--scrub-preset` defaults to `standard` (case-insensitive); invalid preset exits 1.
  - `--dry-run` uses a temp output dir and prints a summary + security checklist; no bundle artifacts written.
  - If `detach_threshold <= inline_threshold`, it auto-adjusts: `detach = inline + max(1024, inline/2)`.
  - `--interactive` runs a wizard that prompts for project filters, thresholds, preset, and zip; returned values override flags.
  - Builds snapshot, scrubs per preset, packages attachments, builds FTS (if available), writes manifest + viewer assets.
  - Optional signing (`--signing-key` Ed25519 seed) and optional `--signing-public-out`.
  - Optional age encryption (`--age-recipient` repeated) applied to the ZIP output.
- `share update`:
  - Loads export config from existing bundle `manifest.json` + stored export config.
  - Overrides allowed for project scope, thresholds, scrub preset; validates non-negative; chunk size >= 1024.
  - Creates temp snapshot, rebuilds bundle assets, then syncs into existing bundle directory.
  - `--zip` default is **false** (contrast with export default **true**).
- `share preview`:
  - Defaults: `--host 127.0.0.1`, `--port 9000`, `--open-browser` default false.
  - Serves static bundle with no-cache headers; listens for `r` (reload), `d` (deploy requested), `q` (quit).
  - Exits with code **42** when deployment was requested via `d`.
- `share verify`:
  - Validates SRI hashes and optional Ed25519 signature; `--public-key` uses provided base64 key or manifest signature.
- `share decrypt`:
  - `--output` defaults to filename without `.age` (or adds `_decrypted` suffix).
  - `--identity` (age key file) is mutually exclusive with `--passphrase`.
- `share wizard`:
  - Launches `scripts/share_to_github_pages.py` from repo (source-only); errors if not found.

### Doctor Commands (CLI)
- `doctor check`:
  - `--json` prints machine-readable diagnostics + summary counts.
  - Checks: stale locks, SQLite `PRAGMA integrity_check`, orphaned message recipients,
    FTS row count mismatch, expired file reservations, WAL/SHM presence.
  - `--verbose` prints up to 5 detail lines per diagnostic.
- `doctor repair`:
  - `--dry-run` previews; `--yes` skips confirmations; `--backup-dir` overrides backup location.
  - Creates a backup (unless dry-run), heals stale locks, releases expired reservations.
  - Data repair: deletes orphaned message recipients (prompt unless `--yes`).
- `doctor backups`:
  - Lists backups (table) or JSON.
- `doctor restore`:
  - Requires `manifest.json` in backup dir; warns about overwriting DB/archive.
  - `--dry-run` prints what would restore; otherwise performs restore and reports results.

### Guard Commands (CLI)
- `guard install`:
  - Requires `WORKTREES_ENABLED=1`; installs pre-commit guard script; optional `--prepush`.
- `guard uninstall`:
  - Resolves hooks dir via `core.hooksPath` or `.git/hooks`, removes guard scripts if present.
- `guard status`:
  - Prints gate/mode, resolved hooks path, and presence of pre-commit/pre-push hooks.
- `guard check`:
  - Requires `AGENT_NAME` env var; reads NUL-delimited paths when `--stdin-nul`.
  - Normalizes to repo-root relative; respects `core.ignorecase`.
  - Uses `pathspec` (gitignore) when available, else fnmatch; symmetric matching.
  - Prints conflicts; non-zero exit unless `--advisory`.

### Build Slot Helpers (CLI)
- `amctl env` prints environment variables:
  - `SLUG`, `PROJECT_UID`, `BRANCH`, `AGENT`, `CACHE_KEY`, `ARTIFACT_DIR`.
- `am-run`:
  - Acquires build slot (server tool if available, else local JSON lease under `build_slots/`).
  - Renewal interval: `max(60, ttl_seconds/2)`; releases slot on exit.
  - `--block-on-conflicts` exits 1 when exclusive conflicts present (unless `--shared`).
  - Exports env vars: `AM_SLOT`, `SLUG`, `PROJECT_UID`, `BRANCH`, `AGENT`, `CACHE_KEY`.

## Data Model (SQLModel)
Source: `src/mcp_agent_mail/models.py`.

### Table: `projects`
- `id` (PK)
- `slug` (unique, indexed)
- `human_key` (indexed)
- `created_at` (UTC naive)

### Table: `products`
- `id` (PK)
- `product_uid` (unique, indexed)
- `name` (unique, indexed)
- `created_at` (UTC naive)

### Table: `product_project_links`
- `id` (PK)
- `product_id` (FK -> products.id)
- `project_id` (FK -> projects.id)
- `created_at` (UTC naive)
- Unique: `(product_id, project_id)`

### Table: `agents`
- `id` (PK)
- `project_id` (FK -> projects.id)
- `name` (indexed)
- `program`
- `model`
- `task_description`
- `inception_ts` (UTC naive)
- `last_active_ts` (UTC naive)
- `attachments_policy` (default: `auto`)
- `contact_policy` (default: `auto`)  # open | auto | contacts_only | block_all
- Unique: `(project_id, name)`

### Table: `messages`
- `id` (PK)
- `project_id` (FK -> projects.id)
- `sender_id` (FK -> agents.id)
- `thread_id` (indexed, optional)
- `subject`
- `body_md`
- `importance` (default: `normal`)
- `ack_required` (default: false)
- `created_ts` (UTC naive)
- `attachments` (JSON array, default `[]`)
- Indexes: `(project_id, created_ts)` and `(project_id, sender_id, created_ts)`

### Table: `message_recipients`
- `message_id` (PK, FK -> messages.id)
- `agent_id` (PK, FK -> agents.id)
- `kind` (default: `to`)  # to | cc | bcc
- `read_ts` (optional)
- `ack_ts` (optional)
- Index: `(agent_id, message_id)`

### Table: `file_reservations`
- `id` (PK)
- `project_id` (FK -> projects.id)
- `agent_id` (FK -> agents.id)
- `path_pattern`
- `exclusive` (default: true)
- `reason` (default: empty)
- `created_ts` (UTC naive)
- `expires_ts`
- `released_ts` (optional)
- Indexes: `(project_id, released_ts, expires_ts)` and `(project_id, agent_id, released_ts)`

### Table: `agent_links`
- `id` (PK)
- `a_project_id` (FK -> projects.id)
- `a_agent_id` (FK -> agents.id)
- `b_project_id` (FK -> projects.id)
- `b_agent_id` (FK -> agents.id)
- `status` (default: `pending`)  # pending | approved | blocked
- `reason` (default: empty)
- `created_ts` (UTC naive)
- `updated_ts` (UTC naive)
- `expires_ts` (optional)
- Unique: `(a_project_id, a_agent_id, b_project_id, b_agent_id)`

### Table: `project_sibling_suggestions`
- `id` (PK)
- `project_a_id` (FK -> projects.id)
- `project_b_id` (FK -> projects.id)
- `score` (float)
- `status` (default: `suggested`)  # suggested | confirmed | dismissed
- `rationale` (default: empty)
- `created_ts` (UTC naive)
- `evaluated_ts` (UTC naive)
- `confirmed_ts` (optional)
- `dismissed_ts` (optional)
- Unique: `(project_a_id, project_b_id)`

## Storage Layout (Git Archive)
Source: `src/mcp_agent_mail/storage.py`.
- Archive is a Git repo under `STORAGE_ROOT`, with per-project subtree `projects/<slug>/`.
- Per-project lock file: `projects/<slug>/.archive.lock`.
- Commit lock file: `.commit.lock` under project root when scoped; otherwise repo root.
- Canonical messages: `projects/<slug>/messages/YYYY/MM/<timestamp>__<subject-slug>__<id>.md`.
- Thread digests: `projects/<slug>/messages/threads/<thread_id>.md`.
- Per-agent inbox/outbox copies: `projects/<slug>/agents/<name>/inbox/YYYY/MM/` and `.../outbox/...`.
- Agent profile: `projects/<slug>/agents/<name>/profile.json`.
- File reservations: `projects/<slug>/file_reservations/<sha1(path_pattern)>.json` and `id-<id>.json`.
- Attachments: `projects/<slug>/attachments/<sha1[:2]>/<sha1>.webp`.
- Originals (optional): `projects/<slug>/attachments/originals/<sha1[:2]>/<sha1>.<ext>`.
- Attachment manifests: `projects/<slug>/attachments/_manifests/<sha1>.json`.
- Attachment audit log: `projects/<slug>/attachments/_audit/<sha1>.log`.
- Notification signals: `{NOTIFICATIONS_SIGNALS_DIR}/projects/<slug>/agents/<agent>.signal`.

### Message File Format
- Frontmatter is JSON with header line `---json`, followed by Markdown body.
- Filenames include ISO timestamp + subject slug + optional message id.

---

## MCP Tools (35 Total)

### Identity Cluster (CLUSTER_IDENTITY)

| Tool | Parameters | Returns | Notes |
|------|------------|---------|-------|
| `health_check` | none | `{status, environment, http_host, http_port, database_url}` | Infrastructure check |
| `ensure_project` | `human_key: str, identity_mode?: str` | `{id, slug, human_key, created_at}` | MUST be absolute path; idempotent |
| `register_agent` | `project_key, program, model, name?, task_description?, attachments_policy?` | Agent record | Upsert semantics; auto-generates name if omitted |
| `create_agent_identity` | Same as register_agent + `name_hint?` | Agent record | Always creates NEW identity |
| `whois` | `project_key, agent_name, include_recent_commits?, commit_limit?` | Agent profile + commits | Lookup with optional Git history |

### Messaging Cluster (CLUSTER_MESSAGING)

| Tool | Parameters | Returns | Notes |
|------|------------|---------|-------|
| `send_message` | `project_key, sender_name, to: list, subject, body_md, cc?, bcc?, attachment_paths?, convert_images?, importance?, ack_required?, thread_id?, auto_contact_if_blocked?` | `{deliveries, count}` | Complex - handles contacts, archives |
| `reply_message` | `project_key, message_id, sender_name, body_md, to?, cc?, bcc?, subject_prefix?` | Message payload | Inherits thread_id, importance, ack_required |
| `fetch_inbox` | `project_key, agent_name, urgent_only?, since_ts?, limit?, include_bodies?` | `list[Message]` | Non-mutating read |
| `mark_message_read` | `project_key, agent_name, message_id` | `{message_id, read, read_at}` | Idempotent |
| `acknowledge_message` | `project_key, agent_name, message_id` | `{message_id, acknowledged, acknowledged_at, read_at}` | Also marks read |

### Contact Cluster (CLUSTER_CONTACT)

| Tool | Parameters | Returns | Notes |
|------|------------|---------|-------|
| `request_contact` | `project_key, from_agent, to_agent, to_project?, reason?, ttl_seconds?, register_if_missing?, program?, model?, task_description?` | `{from, from_project, to, to_project, status, expires_ts?}` | Creates/refreshes a pending AgentLink |
| `respond_contact` | `project_key, to_agent, from_agent, from_project?, accept: bool, ttl_seconds?` | `{from, to, approved, expires_ts?, updated}` | Approve/deny contact request (idempotent) |
| `list_contacts` | `project_key, agent_name` | `list[{to, status, reason, updated_ts?, expires_ts?}]` | Returns outgoing contact links (Python format) |
| `set_contact_policy` | `project_key, agent_name, policy` | `{agent, policy}` | Invalid policy coerces to `auto`; policies: `open\|auto\|contacts_only\|block_all` |

### File Reservation Cluster (CLUSTER_FILE_RESERVATIONS)

| Tool | Parameters | Returns | Notes |
|------|------------|---------|-------|
| `file_reservation_paths` | `project_key, agent_name, paths: list, ttl_seconds?, exclusive?, reason?` | `{granted, conflicts}` | TTL >= 60s; symmetric fnmatch |
| `release_file_reservations` | `project_key, agent_name, paths?, file_reservation_ids?` | `{released, released_at}` | Omit both = release all |
| `renew_file_reservations` | `project_key, agent_name, extend_seconds?, paths?, file_reservation_ids?` | `{renewed, file_reservations}` | Extends from max(now, expiry) |
| `force_release_file_reservation` | `project_key, agent_name, file_reservation_id, note?, notify_previous?` | Release result | Validates inactivity heuristics |
| `install_precommit_guard` | `project_key, code_repo_path` | Install result | Resolves `core.hooksPath` |
| `uninstall_precommit_guard` | `code_repo_path` | Uninstall result | Removes hook |

### Search Cluster (CLUSTER_SEARCH)

| Tool | Parameters | Returns | Notes |
|------|------------|---------|-------|
| `search_messages` | `project_key, query, limit?` | `list[{id, subject, importance, ...}]` | FTS5 syntax: phrases, prefix, boolean |
| `summarize_thread` | `project_key, thread_id, include_examples?, llm_mode?, llm_model?, per_thread_limit?` | Summary with participants, key_points, action_items | Multi-thread: comma-separated IDs |

### Macro Cluster (CLUSTER_MACROS)

| Tool | Parameters | Returns | Notes |
|------|------------|---------|-------|
| `macro_start_session` | `human_key, program, model, agent_name?, task_description?, file_reservation_paths?, file_reservation_reason?, file_reservation_ttl_seconds?, inbox_limit?` | `{project, agent, file_reservations, inbox}` | Composite: ensure_project + register_agent + reserve + fetch_inbox |
| `macro_prepare_thread` | `project_key, thread_id, program, model, agent_name?, task_description?, register_if_missing?, include_examples?, include_inbox_bodies?, llm_mode?, llm_model?, inbox_limit?` | `{agent, thread_summary, inbox}` | Composite: register + summarize + fetch_inbox |
| `macro_file_reservation_cycle` | `project_key, agent_name, paths, ttl_seconds?, exclusive?, reason?, auto_release?` | Reservation result | Composite: reserve + optional release |
| `macro_contact_handshake` | `project_key, requester?, target?, reason?, auto_accept?, ttl_seconds?, welcome_subject?, welcome_body?, thread_id?, register_if_missing?, program?, model?, task_description?` | Handshake result | Composite: request + optional approve + optional welcome |

### Product Cluster (CLUSTER_PRODUCT)

| Tool | Parameters | Returns | Notes |
|------|------------|---------|-------|
| `ensure_product` | `product_key?, name?` | Product record | Auto-generates UID if omitted |
| `products_link` | `product_key, project_keys: list` | Link result | Many-to-many linking |
| `search_messages_product` | `product_key, query, limit?` | Cross-project search | Searches all linked projects |
| `fetch_inbox_product` | `product_key, agent_name, urgent_only?, since_ts?, limit?, include_bodies?` | Cross-project inbox | Aggregates from all linked projects |
| `summarize_thread_product` | `product_key, thread_id, include_examples?, llm_mode?, llm_model?, per_thread_limit?` | Product-scoped summary | Cross-project thread aggregation |

### Build Slot Cluster (CLUSTER_BUILD_SLOTS)

| Tool | Parameters | Returns | Notes |
|------|------------|---------|-------|
| `acquire_build_slot` | `project_key, agent_name, slot_key, ttl_seconds?` | Slot result | Uses file_reservations with `build_slots/{key}.lock` |
| `renew_build_slot` | `project_key, agent_name, slot_key, extend_seconds?` | Renewal result | Extends slot TTL |
| `release_build_slot` | `project_key, agent_name, slot_key` | Release result | Releases slot |

---

## MCP Resources (20+)

### Configuration
- `resource://config/environment{?format}` - Environment snapshot

### Identity
- `resource://identity/{project}{?format}` - Git identity resolution
- `resource://agents/{project_key}{?format}` - Agent list with profiles

### Tooling
- `resource://tooling/directory{?format}` - All tools with cluster/capability metadata
- `resource://tooling/schemas{?format}` - JSON schemas for all tools
- `resource://tooling/metrics{?format}` - Tool call counts, error rates
- `resource://tooling/locks{?format}` - Active archive locks
- `resource://tooling/capabilities/{agent}{?project,format}` - Agent capability mapping
- `resource://tooling/recent/{window_seconds}{?agent,project,format}` - Recent tool usage

### Projects
- `resource://projects{?format}` - All projects
- `resource://project/{slug}{?format}` - Project details + stats
- `resource://product/{key}{?format}` - Product with linked projects

### Messages & Threads
- `resource://message/{message_id}{?project,format}` - Full message details
- `resource://thread/{thread_id}{?project,include_bodies,format}` - Thread messages
- `resource://inbox/{agent}{?project,since_ts,urgent_only,include_bodies,limit,format}` - Inbox view
- `resource://mailbox/{agent}{?project,limit,format}` - Combined inbox/outbox
- `resource://mailbox-with-commits/{agent}{?project,limit,format}` - Mailbox + commit metadata
- `resource://outbox/{agent}{?project,limit,include_bodies,since_ts,format}` - Sent messages

### Views
- `resource://views/urgent-unread/{agent}{?project,limit,format}` - Unread high/urgent
- `resource://views/ack-required/{agent}{?project,limit,format}` - Requiring ack
- `resource://views/acks-stale/{agent}{?project,ttl_seconds,limit,format}` - Unacked > TTL
- `resource://views/ack-overdue/{agent}{?project,ttl_minutes,limit,format}` - Overdue acks

### File Reservations
- `resource://file_reservations/{slug}{?active_only,format}` - Active/all reservations

---

## HTTP Transport Endpoints (HTTP Server)
Source: `src/mcp_agent_mail/http.py`.

- `GET /health/liveness` → `{ "status": "alive" }`
- `GET /health/readiness` → `{ "status": "ready" }` or 503 with error details
- `GET /.well-known/oauth-authorization-server` → `{ "mcp_oauth": false }`
- `GET /.well-known/oauth-authorization-server/mcp` → `{ "mcp_oauth": false }`
- `POST {HTTP_PATH}` → Streamable HTTP MCP transport (JSON-RPC)
- `GET /mail/...` → HTML UI + JSON endpoints (inbox, locks, archive, etc.)

---

## Configuration Environment Variables

### Application
```
APP_ENVIRONMENT=development              # development | production
WORKTREES_ENABLED=false
GIT_IDENTITY_ENABLED=false
PROJECT_IDENTITY_MODE=dir                # dir | git-remote | git-common-dir | git-toplevel
PROJECT_IDENTITY_REMOTE=origin
```

### Database
```
DATABASE_URL=sqlite+aiosqlite:///./storage.sqlite3
DATABASE_ECHO=false
DATABASE_POOL_SIZE=                      # default: None (let SQLAlchemy decide)
DATABASE_MAX_OVERFLOW=                   # default: None
DATABASE_POOL_TIMEOUT=                   # default: None
```

### Storage
```
STORAGE_ROOT=~/.mcp_agent_mail_git_mailbox_repo
GIT_AUTHOR_NAME=mcp-agent
GIT_AUTHOR_EMAIL=mcp-agent@example.com
INLINE_IMAGE_MAX_BYTES=65536             # 64KB
CONVERT_IMAGES=true
KEEP_ORIGINAL_IMAGES=false
ALLOW_ABSOLUTE_ATTACHMENT_PATHS=true     # Dev only
```

### HTTP Transport
```
HTTP_HOST=127.0.0.1
HTTP_PORT=8765
HTTP_PATH=/api/
HTTP_BEARER_TOKEN=
HTTP_ALLOW_LOCALHOST_UNAUTHENTICATED=true

# Rate Limiting
HTTP_RATE_LIMIT_ENABLED=false
HTTP_RATE_LIMIT_BACKEND=memory           # memory | redis
HTTP_RATE_LIMIT_TOOLS_PER_MINUTE=60
HTTP_RATE_LIMIT_RESOURCES_PER_MINUTE=120
HTTP_RATE_LIMIT_TOOLS_BURST=0
HTTP_RATE_LIMIT_RESOURCES_BURST=0
HTTP_RATE_LIMIT_REDIS_URL=

# Request Logging / OTEL
HTTP_REQUEST_LOG_ENABLED=false
HTTP_OTEL_ENABLED=false
OTEL_SERVICE_NAME=mcp-agent-mail
OTEL_EXPORTER_OTLP_ENDPOINT=

# JWT
HTTP_JWT_ENABLED=false
HTTP_JWT_ALGORITHMS=HS256
HTTP_JWT_SECRET=
HTTP_JWT_JWKS_URL=
HTTP_JWT_AUDIENCE=
HTTP_JWT_ISSUER=
HTTP_JWT_ROLE_CLAIM=role

# RBAC
HTTP_RBAC_ENABLED=true
HTTP_RBAC_READER_ROLES=reader,read,ro
HTTP_RBAC_WRITER_ROLES=writer,write,tools,rw
HTTP_RBAC_DEFAULT_ROLE=reader
HTTP_RBAC_READONLY_TOOLS=health_check,fetch_inbox,whois,search_messages,summarize_thread

# CORS
HTTP_CORS_ENABLED=true (development) / false (production)
HTTP_CORS_ORIGINS=
HTTP_CORS_ALLOW_CREDENTIALS=false
HTTP_CORS_ALLOW_METHODS=*
HTTP_CORS_ALLOW_HEADERS=*
```

### Contact & Messaging
```
CONTACT_ENFORCEMENT_ENABLED=true
CONTACT_AUTO_TTL_SECONDS=86400           # 24h
CONTACT_AUTO_RETRY_ENABLED=true
MESSAGING_AUTO_REGISTER_RECIPIENTS=true
MESSAGING_AUTO_HANDSHAKE_ON_BLOCK=true
```

### File Reservations
```
FILE_RESERVATIONS_CLEANUP_ENABLED=false
FILE_RESERVATIONS_CLEANUP_INTERVAL_SECONDS=60
FILE_RESERVATION_INACTIVITY_SECONDS=1800 # 30min
FILE_RESERVATION_ACTIVITY_GRACE_SECONDS=900
FILE_RESERVATIONS_ENFORCEMENT_ENABLED=true
```

### Agent Naming
```
AGENT_NAME_ENFORCEMENT_MODE=coerce       # strict | coerce | always_auto
```

### LLM
```
LLM_ENABLED=true
LLM_DEFAULT_MODEL=gpt-4o-mini
LLM_TEMPERATURE=0.2
LLM_MAX_TOKENS=512
LLM_CACHE_ENABLED=true
LLM_CACHE_BACKEND=memory
LLM_CACHE_REDIS_URL=
LLM_COST_LOGGING_ENABLED=true
```

Provider env bridge (`llm.py:_bridge_provider_env`):
- Mapping (canonical -> aliases):
  - OPENAI_API_KEY -> [OPENAI_API_KEY]
  - ANTHROPIC_API_KEY -> [ANTHROPIC_API_KEY]
  - GROQ_API_KEY -> [GROQ_API_KEY]
  - XAI_API_KEY -> [XAI_API_KEY, GROK_API_KEY]
  - GOOGLE_API_KEY -> [GOOGLE_API_KEY, GEMINI_API_KEY]
  - OPENROUTER_API_KEY -> [OPENROUTER_API_KEY]
  - DEEPSEEK_API_KEY -> [DEEPSEEK_API_KEY]
- Lookup order for each canonical key:
  1) os.environ (canonical or alias)  
  2) .env via decouple (canonical or alias)  
  Canonical is set **only if missing** and a value is found.

Model selection:
- `_resolve_model_alias` maps placeholders {gpt-5-mini, gpt5-mini, gpt-5m, gpt-4o-mini} to `_choose_best_available_model`.
- `_choose_best_available_model`:
  - If preferred contains "/" or ":" -> return as-is.
  - Else choose based on available provider keys (first match wins):
    - OPENAI_API_KEY -> gpt-4o-mini
    - GOOGLE_API_KEY -> gemini-1.5-flash
    - ANTHROPIC_API_KEY -> claude-3-haiku-20240307
    - GROQ_API_KEY -> groq/llama-3.1-70b-versatile
    - DEEPSEEK_API_KEY -> deepseek/deepseek-chat
    - XAI_API_KEY -> xai/grok-2-mini
    - OPENROUTER_API_KEY -> openrouter/openai/gpt-4o-mini
    - Else return preferred.

Caching:
- Enabled when LLM_CACHE_ENABLED=true.
- cache_backend = settings.llm.cache_backend (default "memory") -> lowercased.
- If backend == "redis" and LLM_CACHE_REDIS_URL set:
  - Parse URL; host default "localhost", port default "6379", password from URL.
  - DNS sanity check via socket.gethostbyname(host).
  - On DNS failure -> log "litellm.cache.redis_unavailable_fallback_local", then enable LOCAL cache.
  - On success -> enable Redis cache.
- Otherwise -> enable LOCAL cache.

Cost logging:
- If LLM_COST_LOGGING_ENABLED=true, register a litellm.success_callback.
- Only log when response_cost > 0.
- If LOG_RICH_ENABLED, attempt rich panel output; otherwise structlog.
- All logging errors are suppressed (no impact to normal flow).

Completion helper (`complete_system_user`):
- Uses system/user messages and settings.{temperature,max_tokens} (unless overridden).
- Calls litellm.completion in a thread.
- On error, fall back to `_choose_best_available_model` (if different) and retry once.
- Normalize content:
  - Prefer resp.choices[0].message["content"] (dict) or .content (attr).
  - Else fallback to resp.content.
- Provider/model:
  - provider = resp.provider (if present)
  - model = resp.model (if present) else use selected model

LLM refinement in summaries:
- LLM gating: only when llm_mode=true **and** LLM_ENABLED=true.
- JSON parsing uses `_parse_json_safely`:
  1) raw JSON
  2) fenced JSON block ```json ... ```
  3) brace-slice heuristic
- Single-thread (`_compute_thread_summary`):
  - Prompt expects keys:
    participants[], key_points[], action_items[], mentions[{name,count}],
    code_references[], total_messages, open_actions, done_actions.
  - Merge only truthy values into summary.
  - Heuristic key points containing TODO/ACTION/FIXME/NEXT/BLOCKED are merged (dedup, cap 10).
  - On failure: logger.debug("thread_summary.llm_skipped", {thread_id, error}).
- Multi-thread (`summarize_thread`):
  - Prompt expects JSON:
    {threads:[{thread_id, key_points[], actions[]}],
     aggregate:{top_mentions[], key_points[], action_items[]}}
  - Aggregate keys override if present.
  - Per-thread: if thread_id match, override key_points and action_items (from actions).
  - On failure: ctx.debug("summarize_thread.llm_skipped: {e}")
- Product summarization (`summarize_thread_product`) mirrors the single-thread LLM refinement path.

Testing guidance:
- LLM tests must be offline and stubbed (no network).
- Deterministic fixtures should cover env bridge, model selection, cache fallback, and JSON parsing.

---

### Tool Filtering
Env vars:
```
TOOLS_FILTER_ENABLED=false
TOOLS_FILTER_PROFILE=full
TOOLS_FILTER_MODE=include
TOOLS_FILTER_CLUSTERS=
TOOLS_FILTER_TOOLS=
```

Profiles (from `app.py: TOOL_FILTER_PROFILES`):
```
full:
  clusters=[]
  tools=[]

core:
  clusters=[identity, messaging, file_reservations, workflow_macros]
  tools=[health_check, ensure_project]

minimal:
  clusters=[]
  tools=[health_check, ensure_project, register_agent, send_message, fetch_inbox, acknowledge_message]

messaging:
  clusters=[identity, messaging, contact]
  tools=[health_check, ensure_project, search_messages]
```

Cluster constants:
```
infrastructure, identity, messaging, contact, search,
file_reservations, workflow_macros, build_slots, product_bus
```

Decision logic (`_should_expose_tool`):
1. If filter disabled → expose all tools.
2. Custom profile:
   - clusters_list = settings.tool_filter.clusters
   - tools_list = settings.tool_filter.tools
   - mode = settings.tool_filter.mode (include|exclude)
   - If both lists empty → expose all tools.
   - in_cluster = cluster in clusters_list if clusters_list else False
   - in_tools = tool_name in tools_list if tools_list else False
   - include → expose if in_cluster OR in_tools
   - exclude → expose if NOT (in_cluster OR in_tools)
3. Predefined profile:
   - profile == "full" → expose all tools.
   - Unknown profile → expose all tools.
   - profile_clusters = TOOL_FILTER_PROFILES[profile]["clusters"]
   - profile_tools = TOOL_FILTER_PROFILES[profile]["tools"]
   - If profile_clusters and cluster in profile_clusters → expose.
   - If profile_tools and tool_name in profile_tools → expose.
   - Otherwise expose only if both lists are empty.

Filtering happens once at server startup, not per-request. The legacy server:
- Adds filtered tools to `_FILTERED_TOOLS`
- Removes filtered tools from FastMCP tool registry
- Prunes TOOL_CLUSTER_MAP and TOOL_METADATA
- Logs: `Tool filtering active (profile=...): removed N tools, M tools exposed`

### Notifications
```
NOTIFICATIONS_ENABLED=false
NOTIFICATIONS_SIGNALS_DIR=~/.mcp_agent_mail/signals
NOTIFICATIONS_INCLUDE_METADATA=true
NOTIFICATIONS_DEBOUNCE_MS=100
```

Signal files (legacy `storage.py`):
- Path: `{signals_dir}/projects/{project_slug}/agents/{agent_name}.signal`
- Written as UTF-8 JSON with indent=2.
- Required keys:
  - `timestamp` (UTC `datetime.now(timezone.utc).isoformat()`)
  - `project` (project_slug)
  - `agent` (agent_name)
- Optional `message` (only if NOTIFICATIONS_INCLUDE_METADATA=true and message_metadata passed):
  - `id`, `from`, `subject`, `importance` (defaults to "normal" if missing)

Debounce:
- In-memory map `_SIGNAL_DEBOUNCE[(project_slug, agent_name)] = last_signal_ms`.
- Uses `time.time() * 1000` for current time.
- If `now_ms - last_signal < debounce_ms` → return False (skip emission).
- Debounce timestamp is updated before write; a failed write still counts as “signaled” for debounce.

emit_notification_signal(settings, project_slug, agent_name, message_metadata):
- If notifications disabled → False.
- Builds signals_dir via `Path(...).expanduser().resolve()`.
- Creates parent dirs and writes signal file.
- Best-effort: on any exception → False; never fails message delivery.

clear_notification_signal(settings, project_slug, agent_name):
- If notifications disabled → False.
- If signal file exists → unlink + True; if missing → False.
- Best-effort: exceptions → False.

list_pending_signals(settings, project_slug=None):
- If notifications disabled → [].
- If signals_dir or projects_dir missing → [].
- Reads all `*.signal` files (optionally filtered by project).
- If JSON parse fails → returns minimal dict:
  `{project: <dir>, agent: <stem>, error: "Failed to parse signal file"}`

Integration points (app.py):
- `send_message` emits signals for `to` + `cc` recipients **only** (never `bcc`).
- `fetch_inbox` clears signal best-effort for that agent when notifications enabled.

### Ack TTL / Escalation
```
ACK_TTL_ENABLED=false
ACK_TTL_SECONDS=1800
ACK_TTL_SCAN_INTERVAL_SECONDS=60
ACK_ESCALATION_ENABLED=false
ACK_ESCALATION_MODE=log
ACK_ESCALATION_CLAIM_TTL_SECONDS=3600
ACK_ESCALATION_CLAIM_EXCLUSIVE=false
ACK_ESCALATION_CLAIM_HOLDER_NAME=
```

### Logging / Output
```
LOG_RICH_ENABLED=true
LOG_LEVEL=INFO
LOG_INCLUDE_TRACE=false
LOG_JSON_ENABLED=false
MCP_AGENT_MAIL_OUTPUT_FORMAT=
TOON_DEFAULT_FORMAT=
TOON_STATS=false
TOON_TRU_BIN=
TOON_BIN=
```

### Instrumentation / Metrics
```
TOOLS_LOG_ENABLED=true
INSTRUMENTATION_ENABLED=false
INSTRUMENTATION_SLOW_QUERY_MS=250
TOOL_METRICS_EMIT_ENABLED=false
TOOL_METRICS_EMIT_INTERVAL_SECONDS=60
```

### Retention / Quota
```
RETENTION_REPORT_ENABLED=false
RETENTION_REPORT_INTERVAL_SECONDS=3600
RETENTION_MAX_AGE_DAYS=180
RETENTION_IGNORE_PROJECT_PATTERNS=demo,test*,testproj*,testproject,backendproj*,frontendproj*
QUOTA_ENABLED=false
QUOTA_ATTACHMENTS_LIMIT_BYTES=0
QUOTA_INBOX_LIMIT_COUNT=0
```

## Core Business Logic

### Agent Naming Rules
- MUST be adjective+noun combinations (e.g., "GreenLake", "BlueDog")
- 62 adjectives × 69 nouns = 4,278 valid combinations
- Case-insensitive unique per project
- NOT descriptive role names

### Contact Enforcement Policies
- **`open`:** No restrictions
- **`auto`:** Recent contact within TTL (24h) OR approved link
- **`contacts_only`:** Requires approved link
- **`block_all`:** Rejects all (except self)

**Auto-allowed:**
- Self-send
- Recent thread participants
- Overlapping file reservations

### File Reservation Logic
- Pattern matching: `pathspec` library (gitignore syntax) or `fnmatch` fallback
- Symmetric matching: `fnmatch(pattern, path) OR fnmatch(path, pattern)`
- Exclusive reservations conflict; shared never conflict
- Self-overlap allowed

### Message Archive Flow
1. Validate sender, recipients, contact policy
2. Process attachments (WebP conversion, inline if < 64KB)
3. Write canonical copy: `messages/{YYYY}/{MM}/{timestamp}_{subject_slug}_{id}.md`
4. Write sender outbox: `agents/{sender}/outbox/{YYYY}/{MM}/...`
5. Write recipient inboxes: `agents/{recipient}/inbox/{YYYY}/{MM}/...`
6. Git commit: `"message {id} from {sender} to {recipients}"`
7. Update SQLite index

### Guard Behavior (Pre-Commit / Pre-Push)
- Enabled only when `WORKTREES_ENABLED` or `GIT_IDENTITY_ENABLED` is true.
- `AGENT_MAIL_GUARD_MODE=block` by default; `warn|advisory` for non-blocking.
- `AGENT_MAIL_BYPASS=1` skips checks.
- Requires `AGENT_NAME` env var.
- Pre-commit:
  - Paths from `git diff --cached --name-only -z --diff-filter=ACMRDTU`.
  - Rename pairs from `git diff --cached --name-status -M -z`.
  - Conflicts if exclusive reservation is active, unexpired, and owned by another agent.
  - Uses `pathspec` if installed, else `fnmatch` with symmetric matching.
- Pre-push:
  - Reads refs from stdin and enumerates commits via `git rev-list`.
  - Falls back to `git diff --name-status -M -z <remote>..<local>`.
  - Same conflict logic as pre-commit.
- Install:
  - Installs chain-runner hook; preserves existing hook as `.orig`.
  - Executes `hooks.d/<hook_name>/*` in lexical order.
  - Adds Windows shim for `.cmd`/`.ps1` when needed.

### Thread ID Rules
- Pattern: `^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$`
- Max 128 chars
- Must start with alphanumeric

---

## Git Archive Structure

```
{STORAGE_ROOT}/{project_slug}/
├── agents/
│   └── {AgentName}/
│       ├── profile.json
│       ├── inbox/
│       │   └── {YYYY}/{MM}/{timestamp}_{subject}_{id}.md
│       └── outbox/
│           └── {YYYY}/{MM}/{timestamp}_{subject}_{id}.md
├── messages/
│   └── {YYYY}/{MM}/{timestamp}_{subject}_{id}.md
│   └── threads/{thread_id}.md
├── attachments/
│   ├── {sha1[:2]}/{sha1}.webp
│   ├── originals/{sha1[:2]}/{sha1}.{ext}
│   ├── _manifests/{sha1}.json
│   └── _audit/{sha1}.log
└── file_reservations/
    ├── {sha1(path_pattern)}.json
    └── id-{id}.json
```

---

## SQLite Specifics

### WAL Mode Configuration
- `journal_mode=WAL`
- `synchronous=NORMAL`
- Busy timeout: 60s
- WAL autocheckpoint: 1000 pages (~4MB)
- Page cache: 32MB
- Memory-mapped I/O: 256MB

### FTS5 Virtual Table
```sql
CREATE VIRTUAL TABLE IF NOT EXISTS fts_messages
USING fts5(message_id UNINDEXED, subject, body)
```

### FTS Triggers
- `messages_ai` insert trigger -> inserts into `fts_messages`
- `messages_ad` delete trigger -> deletes from `fts_messages`
- `messages_au` update trigger -> deletes and re-inserts

### Performance Indexes
- `idx_messages_created_ts`
- `idx_messages_thread_id`
- `idx_messages_importance`
- `idx_messages_sender_created`
- `idx_messages_project_created`
- `idx_messages_project_sender_created`
- `idx_file_reservations_expires_ts`
- `idx_file_reservations_project_released_expires`
- `idx_file_reservations_project_agent_released`
- `idx_message_recipients_agent`
- `idx_message_recipients_agent_message`
- `idx_agent_links_a_project`
- `idx_agent_links_b_project`
- `idx_agent_links_status`

---

## Share / Export Pipeline (Static Bundle) — Complete Spec

> **Self-contained reference.** Implementers should be able to complete the entire
> Share/Export feature using ONLY this section (no Python code consultation required).

Source: `src/mcp_agent_mail/share.py` (~2,218 LOC).

### Pipeline Overview

```
1. create_sqlite_snapshot()    — WAL checkpoint + sqlite3.backup
2. apply_project_scope()       — DELETE rows for non-selected projects
3. scrub_snapshot()            — Per-preset redaction (secrets, ack state, etc.)
4. build_search_indexes()      — FTS5 virtual table for static viewer search
5. build_materialized_views()  — Denormalized tables for httpvfs performance
6. create_performance_indexes()— Covering indexes + lowercase columns
7. finalize_snapshot_for_export() — journal_mode=DELETE, page_size=1024, VACUUM, ANALYZE
8. bundle_attachments()        — Inline/file/external/missing classification
9. maybe_chunk_database()      — Split large DB into 4 MiB chunks
10. copy_viewer_assets()       — Static SPA viewer files
11. export_viewer_data()       — Pre-computed JSON for viewer bootstrap
12. write_bundle_scaffolding() — manifest.json, README, _headers, HOW_TO_DEPLOY, .nojekyll, index.html
13. sign_manifest()            — Optional Ed25519 signing
14. encrypt_bundle()           — Optional age encryption
15. package_directory_as_zip() — Deterministic ZIP packaging
```

### Constants

```rust
const INLINE_ATTACHMENT_THRESHOLD: usize = 64 * 1024;       // 64 KiB
const DETACH_ATTACHMENT_THRESHOLD: usize = 25 * 1024 * 1024; // 25 MiB
const DEFAULT_CHUNK_THRESHOLD: usize = 20 * 1024 * 1024;     // 20 MiB
const DEFAULT_CHUNK_SIZE: usize = 4 * 1024 * 1024;           // 4 MiB
const PSEUDONYM_PREFIX: &str = "agent-";
const PSEUDONYM_LENGTH: usize = 12;
```

### Secret Patterns (Regex)

Used by `scrub_snapshot()` to replace secrets with `[REDACTED]`:

| # | Pattern | Description |
|---|---------|-------------|
| 1 | `ghp_[A-Za-z0-9]{36,}` (case-insensitive) | GitHub personal access tokens |
| 2 | `github_pat_[A-Za-z0-9_]{20,}` (case-insensitive) | GitHub fine-grained PATs |
| 3 | `xox[baprs]-[A-Za-z0-9-]{10,}` (case-insensitive) | Slack tokens |
| 4 | `sk-[A-Za-z0-9]{20,}` (case-insensitive) | OpenAI / generic API keys |
| 5 | `(?i)bearer\s+[A-Za-z0-9_\-\.]{16,}` | Bearer tokens |
| 6 | `eyJ[0-9A-Za-z_-]+\.[0-9A-Za-z_-]+\.[0-9A-Za-z_-]+` | JWT tokens (3 base64url segments) |

### Attachment Redact Keys

When scrubbing attachment metadata (JSON dicts), remove these keys entirely:
```
download_url, headers, authorization, signed_url, bearer_token
```
A key is only counted as "removed" if its value is non-empty (not `None`, `""`, `[]`, `{}`).

### Scrub Presets

Three presets, selected by name (case-insensitive, default `"standard"`):

| Preset | `redact_body` | `body_placeholder` | `drop_attachments` | `scrub_secrets` | `clear_ack_state` | `clear_recipients` | `clear_file_reservations` | `clear_agent_links` |
|--------|:---:|---|:---:|:---:|:---:|:---:|:---:|:---:|
| **standard** | false | null | false | true | true | true | true | true |
| **strict** | true | `"[Message body redacted]"` | true | true | true | true | true | true |
| **archive** | false | null | false | false | false | false | false | false |

**Preset descriptions:**
- **standard**: Default redaction. Clear ack/read state, scrub common secrets (API keys, tokens); retain agent names, message bodies and attachments.
- **strict**: High-scrub. Replace message bodies with placeholders and omit all attachments from the snapshot.
- **archive**: Lossless snapshot for disaster recovery. Preserve everything while still running the standard cleanup pipeline.

### Step 1: `create_sqlite_snapshot(source, destination, checkpoint=true)`

1. Resolve `destination` to absolute path; create parent dirs.
2. **Error** if destination already exists (never overwrite).
3. Open `source` connection.
4. If `checkpoint`: execute `PRAGMA wal_checkpoint(PASSIVE)`.
5. Open `destination` connection.
6. Use `sqlite3.backup()` API to copy source → destination.
7. Close both connections.
8. Return destination path.

### Step 2: `apply_project_scope(snapshot_path, identifiers: &[String])`

Returns `ProjectScopeResult { projects: Vec<ProjectRecord>, removed_count: usize }`.

1. Open snapshot, set `PRAGMA foreign_keys=ON`, set `row_factory`.
2. Load all projects: `SELECT id, slug, human_key FROM projects`.
3. **Error** if no projects exist.
4. If `identifiers` is empty → return all projects with `removed_count = 0`.
5. Build lookup map: `slug.lower()` → record, `human_key.lower()` → record.
6. Match identifiers (case-insensitive, trimmed). **Error** if any not found.
7. Compute `allowed_ids` (set) and `disallowed_ids` (list).
8. If no disallowed → return early.
9. **Delete order** (uses `NOT IN (allowed_ids)` with `?` placeholders):
   ```sql
   DELETE FROM agent_links WHERE a_project_id NOT IN (...) OR b_project_id NOT IN (...)
   DELETE FROM project_sibling_suggestions WHERE project_a_id NOT IN (...) OR project_b_id NOT IN (...)
   -- Collect message IDs first:
   SELECT id FROM messages WHERE project_id NOT IN (...)
   DELETE FROM message_recipients WHERE message_id IN (<collected_ids>)
   DELETE FROM messages WHERE project_id NOT IN (...)
   DELETE FROM file_reservations WHERE project_id NOT IN (...)
   DELETE FROM agents WHERE project_id NOT IN (...)
   DELETE FROM projects WHERE id NOT IN (...)
   ```
10. Commit. Return result.

### Step 3: `scrub_snapshot(snapshot_path, preset, export_salt)`

Returns `ScrubSummary` with 12 fields.

**ScrubSummary fields:**
```
preset, pseudonym_salt, agents_total, agents_pseudonymized,
ack_flags_cleared, recipients_cleared, file_reservations_removed,
agent_links_removed, secrets_replaced, attachments_sanitized,
bodies_redacted, attachments_cleared
```

**Algorithm:**
1. Open snapshot, set `PRAGMA foreign_keys=ON`.
2. Count agents: `SELECT COUNT(*) FROM agents` → `agents_total`. Set `agents_pseudonymized = 0`.
3. If `clear_ack_state`: `UPDATE messages SET ack_required = 0` → count = `ack_flags_cleared`.
4. If `clear_recipients`: `UPDATE message_recipients SET read_ts = NULL, ack_ts = NULL` → count = `recipients_cleared`.
5. If `clear_file_reservations`: `DELETE FROM file_reservations` → count = `file_reservations_removed`.
6. If `clear_agent_links`: `DELETE FROM agent_links` → count = `agent_links_removed`.
7. Iterate all messages (`SELECT id, subject, body_md, attachments FROM messages`):
   a. If `scrub_secrets`: apply secret regex patterns to `subject` and `body_md`, replacing matches with `[REDACTED]`.
   b. Parse `attachments` JSON (handle string or list; malformed → empty list).
   c. If `drop_attachments` and attachments non-empty: clear to `[]`, increment `attachments_cleared`.
   d. If `scrub_secrets` and attachments non-empty: recursively scrub structure:
      - Strings: apply secret patterns.
      - Dicts: remove keys in `ATTACHMENT_REDACT_KEYS`.
      - Lists: recurse into each item.
   e. If `redact_body`: replace `body_md` with `body_placeholder` (default `"[Message body redacted]"`), increment `bodies_redacted`.
   f. Write back changed fields via UPDATE.
8. Commit. Set `pseudonym_salt = preset_key`. Return ScrubSummary.

### Step 4: `build_search_indexes(snapshot_path) -> bool`

Creates FTS5 virtual table for full-text search. Returns `true` on success, `false` if FTS5 unavailable.

```sql
CREATE VIRTUAL TABLE IF NOT EXISTS fts_messages USING fts5(
    subject,
    body,
    importance UNINDEXED,
    project_slug UNINDEXED,
    thread_key UNINDEXED,
    created_ts UNINDEXED
);

DELETE FROM fts_messages;

-- If messages table has thread_id column:
INSERT INTO fts_messages(rowid, subject, body, importance, project_slug, thread_key, created_ts)
SELECT
    m.id,
    COALESCE(m.subject, ''),
    COALESCE(m.body_md, ''),
    COALESCE(m.importance, ''),
    COALESCE(p.slug, ''),
    CASE
        WHEN m.thread_id IS NULL OR m.thread_id = '' THEN printf('msg:%d', m.id)
        ELSE m.thread_id
    END,
    COALESCE(m.created_ts, '')
FROM messages AS m
LEFT JOIN projects AS p ON p.id = m.project_id;

-- If messages table lacks thread_id column:
-- Use printf('msg:%d', m.id) for thread_key unconditionally.

INSERT INTO fts_messages(fts_messages) VALUES('optimize');
```

### Step 5: `build_materialized_views(snapshot_path)`

Creates three materialized view tables:

**5a. `message_overview_mv`** — Denormalized message list with sender info.

```sql
DROP TABLE IF EXISTS message_overview_mv;
CREATE TABLE message_overview_mv AS
SELECT
    m.id,
    m.project_id,
    m.thread_id,                    -- or printf('msg:%d', m.id) if no thread_id column
    m.subject,
    m.importance,
    m.ack_required,
    m.created_ts,
    a.name AS sender_name,          -- or '' if no sender_id column
    LENGTH(m.body_md) AS body_length,
    json_array_length(m.attachments) AS attachment_count,
    SUBSTR(COALESCE(m.body_md, ''), 1, 280) AS latest_snippet,
    COALESCE(r.recipients, '') AS recipients
FROM messages m
JOIN agents a ON m.sender_id = a.id  -- omit if no sender_id
LEFT JOIN (
    SELECT mr.message_id,
           GROUP_CONCAT(COALESCE(ag.name, ''), ', ') AS recipients
    FROM message_recipients mr
    LEFT JOIN agents ag ON ag.id = mr.agent_id
    GROUP BY mr.message_id
) r ON r.message_id = m.id
ORDER BY m.created_ts DESC;

CREATE INDEX idx_msg_overview_created ON message_overview_mv(created_ts DESC);
CREATE INDEX idx_msg_overview_thread ON message_overview_mv(thread_id, created_ts DESC);
CREATE INDEX idx_msg_overview_project ON message_overview_mv(project_id, created_ts DESC);
CREATE INDEX idx_msg_overview_importance ON message_overview_mv(importance, created_ts DESC);
```

**5b. `attachments_by_message_mv`** — Flattened JSON attachments.

```sql
DROP TABLE IF EXISTS attachments_by_message_mv;
CREATE TABLE attachments_by_message_mv AS
SELECT
    m.id AS message_id,
    m.project_id,
    m.thread_id,                    -- or NULL if no thread_id column
    m.created_ts,
    json_extract(value, '$.type') AS attachment_type,
    json_extract(value, '$.media_type') AS media_type,
    json_extract(value, '$.path') AS path,
    CAST(json_extract(value, '$.bytes') AS INTEGER) AS size_bytes
FROM messages m,
     json_each(m.attachments)
WHERE m.attachments != '[]';

CREATE INDEX idx_attach_by_msg ON attachments_by_message_mv(message_id);
CREATE INDEX idx_attach_by_type ON attachments_by_message_mv(attachment_type, created_ts DESC);
CREATE INDEX idx_attach_by_project ON attachments_by_message_mv(project_id, created_ts DESC);
```

**5c. `fts_search_overview_mv`** — Pre-computed search result snippets (only if FTS5 available).

```sql
DROP TABLE IF EXISTS fts_search_overview_mv;
CREATE TABLE fts_search_overview_mv AS
SELECT
    m.rowid,
    m.id,
    m.subject,
    m.created_ts,
    m.importance,
    a.name AS sender_name,
    SUBSTR(m.body_md, 1, 200) AS snippet
FROM messages m
JOIN agents a ON m.sender_id = a.id
ORDER BY m.created_ts DESC;

CREATE INDEX idx_fts_overview_rowid ON fts_search_overview_mv(rowid);
CREATE INDEX idx_fts_overview_created ON fts_search_overview_mv(created_ts DESC);
```

### Step 6: `create_performance_indexes(snapshot_path)`

Adds lowercase columns and covering indexes for the static viewer.

```sql
-- Add columns (suppress error if already exist)
ALTER TABLE messages ADD COLUMN subject_lower TEXT;
ALTER TABLE messages ADD COLUMN sender_lower TEXT;

-- Populate (if sender_id column exists)
UPDATE messages
SET
    subject_lower = LOWER(COALESCE(subject, '')),
    sender_lower = LOWER(
        COALESCE(
            (SELECT name FROM agents WHERE agents.id = messages.sender_id),
            ''
        )
    );

-- If no sender_id: sender_lower = ''

CREATE INDEX IF NOT EXISTS idx_messages_created_ts ON messages(created_ts DESC);
CREATE INDEX IF NOT EXISTS idx_messages_subject_lower ON messages(subject_lower);
CREATE INDEX IF NOT EXISTS idx_messages_sender_lower ON messages(sender_lower);
CREATE INDEX IF NOT EXISTS idx_messages_sender ON messages(sender_id, created_ts DESC);
CREATE INDEX IF NOT EXISTS idx_messages_thread ON messages(thread_id, created_ts DESC);
```

### Step 7: `finalize_snapshot_for_export(snapshot_path)`

```sql
PRAGMA journal_mode=DELETE;   -- Single-file mode (no -wal/-shm)
PRAGMA page_size=1024;        -- httpvfs-friendly page size
VACUUM;                       -- Compact database, apply new page_size
PRAGMA analysis_limit=400;    -- Limit rows sampled for ANALYZE
ANALYZE;                      -- Update query planner statistics
PRAGMA optimize;              -- Optimize query planner
```

### Step 8: `bundle_attachments(snapshot_path, output_dir, storage_root, inline_threshold, detach_threshold)`

Returns attachment manifest dict:
```json
{
  "stats": {
    "inline": <count>,
    "copied": <count>,
    "externalized": <count>,
    "missing": <count>,
    "bytes_copied": <total_bytes>
  },
  "config": {
    "inline_threshold": <bytes>,
    "detach_threshold": <bytes>
  },
  "items": [ <per-attachment records> ]
}
```

**Per-attachment classification logic:**

For each `messages.attachments` JSON entry where `type == "file"`:

1. Resolve `path` (relative paths resolved against `storage_root`).
2. If file doesn't exist → **missing**:
   - Rewrite entry: `{ "type": "missing", "original_path", "media_type", "sha_hint" }`
3. Read file, compute SHA256 and size.
4. If `size <= inline_threshold` → **inline**:
   - Base64-encode content.
   - Rewrite entry: `{ "type": "inline", "media_type", "bytes", "sha256", "data_uri": "data:{media_type};base64,{encoded}" }`
5. If `size >= detach_threshold` → **external**:
   - Rewrite entry: `{ "type": "external", "media_type", "bytes", "sha256", "original_path", "note": "Requires manual hosting (exceeds bundle threshold)." }`
6. Otherwise → **file** (copy to bundle):
   - Content-addressed dedup: `attachments/{sha256[:2]}/{sha256}{ext}`
   - Only copy if not already present (dedup by SHA256).
   - Rewrite entry: `{ "type": "file", "media_type", "bytes", "sha256", "path": "<relative_posix_path>" }`

After processing all messages, write updated `messages.attachments` JSON back to snapshot DB and commit.

### Step 9: `maybe_chunk_database(snapshot_path, output_dir, threshold_bytes, chunk_bytes)`

Returns `None` if DB size <= threshold. Otherwise:

1. Create `chunks/` directory in output_dir.
2. Read snapshot in `chunk_bytes` chunks.
3. Write each chunk as `chunks/{index:05d}.bin`.
4. Compute SHA256 for each chunk.
5. Write `chunks.sha256` file with lines: `{sha256}  chunks/{filename}\n`.
6. Write `mailbox.sqlite3.config.json`:
   ```json
   {
     "version": 1,
     "chunk_size": <chunk_bytes>,
     "chunk_count": <count>,
     "pattern": "chunks/{index:05d}.bin",
     "original_bytes": <total_size>,
     "threshold_bytes": <threshold>
   }
   ```

### Step 10: `copy_viewer_assets(output_dir)`

Copies viewer SPA files into `{output_dir}/viewer/`.

Priority order:
1. Source tree: `{share.py parent}/viewer_assets/` → copy recursively.
2. Fallback: packaged resources via `importlib.resources` (verifies vendor asset SHA256 checksums first).

### Step 11: `export_viewer_data(snapshot_path, output_dir, limit=500, fts_enabled=false)`

Pre-computes JSON data for viewer bootstrap.

1. Query: `SELECT id, subject, body_md, created_ts, importance, project_id FROM messages ORDER BY created_ts DESC LIMIT ?`
2. For each row, create snippet: `body.strip().replace("\n", " ")[:280]`.
3. Write `viewer/data/messages.json`: array of `{ id, subject, created_ts, importance, project_id, snippet }`.
4. Write `viewer/data/meta.json`: `{ generated_at, message_count, messages_cached, fts_enabled }`.
5. Return `{ messages: <relative_path>, meta: <relative_path>, meta_info: <meta_dict> }`.

### Step 12: `write_bundle_scaffolding(...)`

Writes the following files into `output_dir`:

**12a. `manifest.json`** — Machine-readable metadata.

```json
{
  "schema_version": "0.1.0",
  "generated_at": "<ISO-8601 UTC>",
  "exporter_version": "prototype",
  "database": {
    "path": "mailbox.sqlite3",
    "size_bytes": <int>,
    "sha256": "<hex>",
    "chunked": <bool>,
    "chunk_manifest": <null | chunk_config>,
    "fts_enabled": <bool>
  },
  "project_scope": {
    "requested": ["<filter1>", ...],
    "included": [{"slug": "...", "human_key": "..."}],
    "removed_count": <int>
  },
  "scrub": {
    "preset": "<name>",
    "pseudonym_salt": "<preset_name>",
    "agents_total": <int>,
    "agents_pseudonymized": <int>,
    "ack_flags_cleared": <int>,
    "recipients_cleared": <int>,
    "file_reservations_removed": <int>,
    "agent_links_removed": <int>,
    "secrets_replaced": <int>,
    "attachments_sanitized": <int>,
    "bodies_redacted": <int>,
    "attachments_cleared": <int>
  },
  "attachments": { <attachment_manifest> },
  "hosting": {
    "detected": [
      {
        "id": "<key>",
        "title": "<name>",
        "summary": "<description>",
        "signals": ["<evidence1>", ...]
      }
    ]
  },
  "viewer": {
    "messages": "viewer/data/messages.json",
    "meta": "viewer/data/meta.json",
    "meta_info": { "generated_at", "message_count", "messages_cached", "fts_enabled" },
    "sri": { "<relative_path>": "sha256-<base64>" }
  },
  "export_config": {
    "projects": [...],
    "scrub_preset": "...",
    "inline_threshold": ...,
    "detach_threshold": ...,
    ...
  },
  "notes": [
    "Prototype manifest. Viewer asset Subresource Integrity hashes recorded under viewer.sri.",
    "Viewer scaffold with diagnostics is bundled; SPA search/thread views arrive in upcoming milestones."
  ]
}
```

**12b. `README.md`** — Human-readable overview with Quick Start, deployment instructions, troubleshooting.

**12c. `index.html`** — Redirect page: `<meta http-equiv="refresh" content="0; url=./viewer/" />` with styled loading page and JS fallback.

**12d. `.nojekyll`** — Empty file (disables Jekyll on GitHub Pages).

**12e. `HOW_TO_DEPLOY.md`** — Generated from hosting hints + `HOSTING_GUIDES` + `GENERIC_HOSTING_NOTES`.

**12f. `_headers`** — COOP/COEP headers for Cloudflare Pages / Netlify:
```
# Cross-Origin Isolation headers for OPFS and SharedArrayBuffer support
# Compatible with Cloudflare Pages and Netlify

/*
  Cross-Origin-Opener-Policy: same-origin
  Cross-Origin-Embedder-Policy: require-corp

/viewer/*
  Cross-Origin-Resource-Policy: same-origin

/*.sqlite3
  Cross-Origin-Resource-Policy: same-origin
  Content-Type: application/x-sqlite3

/chunks/*
  Cross-Origin-Resource-Policy: same-origin
  Content-Type: application/octet-stream

/attachments/*
  Cross-Origin-Resource-Policy: same-origin
```

### Step 13: `sign_manifest(manifest_path, signing_key_path, output_path, public_out?)`

Optional Ed25519 signing via PyNaCl (Rust: use `ed25519-dalek` or similar).

1. Read manifest bytes and signing key bytes.
2. Key must be 32 or 64 bytes (use first 32 as seed).
3. Sign manifest bytes → 64-byte signature.
4. Write `manifest.sig.json`:
   ```json
   {
     "algorithm": "ed25519",
     "signature": "<base64>",
     "manifest_sha256": "<hex>",
     "public_key": "<base64>",
     "generated_at": "<ISO-8601 UTC>"
   }
   ```
5. Optionally write public key (base64) to `public_out` file.

### Step 14: `encrypt_bundle(bundle_path, recipients: &[String]) -> Option<Path>`

1. If no recipients → return None.
2. Find `age` CLI in PATH. **Error** if not found.
3. Run: `age -r <recipient1> -r <recipient2> ... -o <bundle_path>.age <bundle_path>`
4. Return encrypted path (original path + `.age` suffix).

### Step 15: `package_directory_as_zip(source_dir, destination) -> Path`

Deterministic ZIP archive:

1. **Error** if source is not a directory or destination already exists.
2. Create ZIP with `ZIP_DEFLATED` compression, level 9.
3. Sort all files by relative path (POSIX separators).
4. For each file:
   - `date_time = (1980, 1, 1, 0, 0, 0)` — normalized for reproducibility.
   - `external_attr = (file_mode & 0o777) << 16` — preserve POSIX permissions.
   - Read and write in 1 MiB chunks.

### Hosting Hints Detection

`detect_hosting_hints(output_dir)` checks for deployment signals:

| Host | Signal Sources |
|------|---------------|
| **GitHub Pages** | git remote contains `github.com`; `.github/workflows/*.yml` references `github-pages`; `GITHUB_REPOSITORY` env var; output inside `docs/` dir |
| **Cloudflare Pages** | git remote contains `cloudflare`; `wrangler.toml` exists; `CF_PAGES` or `CF_ACCOUNT_ID` env vars |
| **Netlify** | git remote contains `netlify`; `netlify.toml` exists; `NETLIFY` or `NETLIFY_SITE_ID` env vars |
| **S3** | git remote contains `amazonaws` or `s3`; `deploy/s3` or `deploy/aws` dir exists; `AWS_S3_BUCKET` or `AWS_BUCKET` env vars |

Sort order: `github_pages, cloudflare_pages, netlify, s3` (preferred first).

### Viewer SRI (Subresource Integrity)

After copying viewer assets, compute SHA256 SRI hashes for `.js`, `.css`, `.wasm` files:
- Format: `"sha256-{base64_of_sha256_digest}"`
- Stored in `manifest.json` under `viewer.sri` as `{ "viewer/path/file.js": "sha256-..." }`.

### Verify Bundle

`verify_bundle(bundle_path, public_key?)`:

1. Read `manifest.json`.
2. For each SRI entry in `viewer.sri`: recompute `sha256-{base64}` and compare. Collect failures.
3. If `manifest.sig.json` exists (or `public_key` provided):
   a. Read signature payload.
   b. Verify Ed25519 signature of manifest bytes using public key.
   c. **Error** on `BadSignatureError`.
4. If SRI failures → **Error** with all failures listed.
5. Return `{ bundle, sri_checked, signature_checked, signature_verified }`.

### Decrypt Bundle

`decrypt_with_age(encrypted_path, output_path, identity?, passphrase?)`:

1. **Error** if both `identity` and `passphrase` provided.
2. **Error** if neither provided.
3. Find `age` CLI. **Error** if not found.
4. Run: `age -d -o <output> [-i <identity> | -p] <encrypted_path>`
   - For passphrase: pipe `passphrase + "\n"` to stdin.

### CLI Interface

| Command | Default | Notes |
|---------|---------|-------|
| `share export --output/-o` | required | Directory for bundle output |
| `--interactive/-i` | false | Launch wizard for project/threshold selection |
| `--project/-p` | [] (all) | Repeatable; filter by slug or human_key |
| `--inline-threshold` | 65536 | Inline attachments <= this (bytes) |
| `--detach-threshold` | 26214400 | Externalize attachments >= this (bytes) |
| `--scrub-preset` | "standard" | standard / strict / archive |
| `--chunk-threshold` | 20971520 | Chunk DB if larger (bytes) |
| `--chunk-size` | 4194304 | Size per chunk (min 1024) |
| `--dry-run/--no-dry-run` | false | Summary only, no artifacts written |
| `--zip/--no-zip` | **true** | Package as ZIP |
| `--signing-key` | None | Ed25519 key path (32-byte seed) |
| `--signing-public-out` | None | Write public key to file |
| `--age-recipient` | [] | Repeatable; encrypt ZIP with age |

**CLI behavior details:**
- If `detach_threshold <= inline_threshold`: auto-adjust `detach = inline + max(1024, inline / 2)`.
- Invalid scrub preset → exit code 1.
- `--dry-run` creates snapshot in temp dir, prints summary + security checklist, cleans up.
- `share update` defaults `--zip` to **false** (contrast with export).
- `share preview` defaults: host=127.0.0.1, port=9000, no auto-open. Keys: `r`=reload, `d`=deploy (exit 42), `q`=quit.
- `share verify` validates SRI hashes + optional Ed25519 signature.
- `share decrypt` resolves output from filename (strip `.age` or add `_decrypted`). Identity and passphrase are mutually exclusive.
- `share wizard` launches `scripts/share_to_github_pages.py` (source-only; errors if not found).

### Bundle Directory Layout

```
{output_dir}/
├── manifest.json
├── manifest.sig.json          (optional, if signed)
├── README.md
├── HOW_TO_DEPLOY.md
├── index.html                 (redirect to viewer/)
├── .nojekyll
├── _headers                   (COOP/COEP)
├── mailbox.sqlite3            (scrubbed snapshot)
├── mailbox.sqlite3.config.json (if chunked)
├── chunks/                    (if chunked)
│   ├── 00000.bin
│   ├── 00001.bin
│   └── ...
├── chunks.sha256              (if chunked)
├── attachments/               (bundled files)
│   └── {sha256[:2]}/{sha256}.{ext}
└── viewer/
    ├── index.html
    ├── data/
    │   ├── messages.json
    │   └── meta.json
    └── ... (SPA assets)
```

## HTTP Background Workers — Complete Spec

> **Self-contained reference.** Implementers should be able to complete the entire
> background worker subsystem using ONLY this section (no Python code consultation required).

Source: `src/mcp_agent_mail/http.py` (lines 554–900), `src/mcp_agent_mail/app.py` (`_tool_metrics_snapshot`, `_expire_stale_file_reservations`).

### Architecture

The HTTP server spawns up to 5 background tasks at startup within the FastAPI/ASGI lifespan.
Each worker runs in an infinite loop with a configurable sleep interval.

**Critical invariant:** Workers MUST never crash the server. Every worker body is wrapped in
`try/except Exception: pass` at the outermost level. Individual sub-operations also suppress
exceptions independently via `contextlib.suppress(Exception)`.

### Startup Conditions

At startup (`_startup()`), workers are spawned only if at least one enable flag is true.
If none are enabled, `_background_tasks` is set to an empty list and the function returns immediately.

| Worker | Enable Flag | Task Creation Condition |
|---|---|---|
| `_worker_cleanup` | `FILE_RESERVATIONS_CLEANUP_ENABLED` | Flag is true |
| `_worker_ack_ttl` | `ACK_TTL_ENABLED` | Flag is true |
| `_worker_tool_metrics` | `TOOL_METRICS_EMIT_ENABLED` | Flag is true |
| `_worker_retention_quota` | `RETENTION_REPORT_ENABLED` **or** `QUOTA_ENABLED` | Either flag is true |

Tasks are stored in `fastapi_app.state._background_tasks` for shutdown cancellation.

### Shutdown

On shutdown (`_shutdown()`), each background task is cancelled via `task.cancel()`,
then awaited with `contextlib.suppress(Exception)` to absorb `CancelledError`.

### Worker 1: File Reservations Cleanup (`_worker_cleanup`)

**Interval:** `FILE_RESERVATIONS_CLEANUP_INTERVAL_SECONDS` (default: 60s)

**Algorithm:**
1. Ensure DB schema is initialized
2. Query distinct project IDs with active file reservations:
   ```sql
   SELECT DISTINCT project_id FROM file_reservations
   ```
3. For each project (suppressing errors individually):
   - Call `_expire_stale_file_reservations(project_id)`
   - Accumulate count of released reservations
4. Log via Rich Panel (if available):
   ```
   title: "File Reservations Cleanup"
   body:  "projects_scanned={n} released={n}"
   border_style: "cyan"
   ```
5. Log via structlog: `"file_reservations_cleanup"` with `projects_scanned` and `stale_released`

**`_expire_stale_file_reservations(project_id)` — Two-Phase Release:**

Phase 1 — Release expired (TTL elapsed):
```sql
-- Find expired
SELECT fr.*, a.* FROM file_reservations fr
JOIN agents a ON fr.agent_id = a.id
WHERE fr.project_id = :pid
  AND fr.released_ts IS NULL
  AND fr.expires_ts < :now

-- Release expired
UPDATE file_reservations
SET released_ts = :now
WHERE project_id = :pid
  AND released_ts IS NULL
  AND expires_ts < :now
```

Phase 2 — Release stale (inactivity heuristics):
- Collect active (non-released) reservations with their `FileReservationStatus`
- Filter for those flagged as `stale` (based on 4-signal heuristics: agent last_active, mail activity, git commits, expiry proximity)
- Release by ID:
```sql
UPDATE file_reservations
SET released_ts = :now
WHERE project_id = :pid
  AND id IN (:stale_ids)
  AND released_ts IS NULL
```

Both phases write archive artifacts (JSON files under `file_reservations/`) for released reservations.

### Worker 2: ACK TTL (`_worker_ack_ttl`)

**Interval:** `ACK_TTL_SCAN_INTERVAL_SECONDS` (default: 60s)

**Algorithm:**
1. Ensure DB schema
2. Query all unacknowledged `ack_required` messages:
   ```sql
   SELECT m.id, m.project_id, m.created_ts, mr.agent_id
   FROM messages m
   JOIN message_recipients mr ON mr.message_id = m.id
   WHERE m.ack_required = 1 AND mr.ack_ts IS NULL
   ```
3. For each row, compute `age = now_utc - created_ts` (handling naive/aware timezone normalization)
4. If `age >= ACK_TTL_SECONDS`:
   - Log via Rich Panel:
     ```
     title: "ACK Overdue"
     border_style: "red"
     body: message_id, agent_id, project_id, age_s, ttl_s
     ```
   - Fallback plain print:
     ```
     ack-warning message_id={mid} project_id={pid} agent_id={aid} age_s={age} ttl_s={ttl}
     ```
   - Log via structlog: `"ack_overdue"` (WARNING level)
   - If `ACK_ESCALATION_ENABLED`, dispatch to escalation handler

#### ACK Escalation: `file_reservation` Mode

When `ACK_ESCALATION_MODE = "file_reservation"` (the only non-log mode):

1. **Compute inbox path pattern** from `created_ts`:
   ```
   agents/{recipient_name}/inbox/{YYYY}/{MM}/*.md
   ```
   - `YYYY` and `MM` from `created_ts.strftime("%Y")` / `created_ts.strftime("%m")`
   - If recipient name cannot be resolved: `agents/*/inbox/{YYYY}/{MM}/*.md`

2. **Resolve recipient name:**
   ```sql
   SELECT name FROM agents WHERE id = :agent_id
   ```

3. **Resolve holder agent** (who will own the file reservation):
   - Default: the recipient agent (same `agent_id`)
   - If `ACK_ESCALATION_CLAIM_HOLDER_NAME` is set:
     - Look up that agent by name:
       ```sql
       SELECT id FROM agents WHERE project_id = :pid AND name = :holder_name
       ```
     - **If not found, auto-create the ops holder:**
       ```sql
       INSERT OR IGNORE INTO agents(
           project_id, name, program, model, task_description,
           inception_ts, last_active_ts, attachments_policy, contact_policy
       ) VALUES (
           :pid, :holder_name, 'ops', 'system', 'ops-escalation',
           :now, :now, 'auto', 'auto'
       )
       ```
     - Also write `profile.json` to archive: `agents/{holder_name}/profile.json`
       ```json
       {
           "id": "<holder_id>",
           "name": "<holder_name>",
           "program": "ops",
           "model": "system",
           "task_description": "ops-escalation",
           "inception_ts": "<iso8601>",
           "last_active_ts": "<iso8601>",
           "project_id": "<pid>",
           "attachments_policy": "auto",
           "contact_policy": "auto"
       }
       ```

4. **Create file reservation in DB:**
   ```sql
   INSERT INTO file_reservations(
       project_id, agent_id, path_pattern, exclusive, reason, created_ts, expires_ts
   ) VALUES (
       :pid,
       :holder_agent_id,
       :pattern,                                    -- e.g. 'agents/BlueLake/inbox/2026/02/*.md'
       :exclusive,                                  -- ACK_ESCALATION_CLAIM_EXCLUSIVE (default: 0)
       'ack-overdue',
       :now,
       :now + ACK_ESCALATION_CLAIM_TTL_SECONDS      -- default: +3600s
   )
   ```

5. **Write archive artifact** via `write_file_reservation_record()`:
   ```json
   {
       "project": "<project_slug>",
       "agent": "<holder_name>",
       "path_pattern": "agents/{name}/inbox/{YYYY}/{MM}/*.md",
       "exclusive": false,
       "reason": "ack-overdue",
       "created_ts": "<iso8601>",
       "expires_ts": "<iso8601>"
   }
   ```

**Error handling:** The entire escalation block is wrapped in `try/except Exception: pass` —
escalation failure never prevents continued scanning.

### Worker 3: Tool Metrics (`_worker_tool_metrics`)

**Interval:** `max(5, TOOL_METRICS_EMIT_INTERVAL_SECONDS)` (default: 60s, minimum: 5s)

**Algorithm:**
1. Call `_tool_metrics_snapshot()`:
   - Iterates `TOOL_METRICS` dict (populated by tool call instrumentation)
   - For each tool name (sorted alphabetically), produces:
     ```json
     {
         "name": "<tool_name>",
         "calls": 0,
         "errors": 0,
         "cluster": "<cluster_name or 'unclassified'>",
         "capabilities": ["<cap1>"],
         "complexity": "<simple|moderate|complex|unknown>"
     }
     ```
2. If snapshot is non-empty, log via structlog:
   ```
   logger: "tool.metrics"
   event: "tool_metrics_snapshot"
   tools: [<snapshot array>]
   ```

### Worker 4: Retention & Quota Report (`_worker_retention_quota`)

**Interval:** `max(60, RETENTION_REPORT_INTERVAL_SECONDS)` (default: 3600s, minimum: 60s)

**Enabled when:** `RETENTION_REPORT_ENABLED=true` **or** `QUOTA_ENABLED=true`

**Algorithm:**
1. Resolve `STORAGE_ROOT` path (expanduser + resolve)
2. Compute cutoff: `now_utc - RETENTION_MAX_AGE_DAYS` (default: 180 days)
3. Compile ignore patterns from `RETENTION_IGNORE_PROJECT_PATTERNS`
   - Default: `demo,test*,testproj*,testproject,backendproj*,frontendproj*`
4. For each project directory under storage root (skipping non-dirs and ignored patterns):
   - **Old messages:** Walk `messages/{YYYY}/{MM}/*.md`, count files with `mtime < cutoff`
   - **Inbox counts:** Walk `agents/*/inbox/*/*/*.md`, count all `.md` files
   - **Attachment bytes:** Walk `attachments/**/*.webp`, sum `stat().st_size` per project
5. Log via structlog (`"maintenance"` logger, INFO level):
   ```
   event: "retention_quota_report"
   old_messages: <int>
   retention_max_age_days: <int>
   total_attachments_bytes: <int>
   quota_limit_bytes: <int>
   per_project_attach: {<project>: <bytes>}
   per_project_inbox_counts: {<project>: <count>}
   ```
6. **Quota alerts** (WARNING level):
   - If `QUOTA_ATTACHMENTS_LIMIT_BYTES > 0` and project attachment bytes >= limit:
     ```
     event: "quota_attachments_exceeded"
     project, used_bytes, limit_bytes
     ```
   - If `QUOTA_INBOX_LIMIT_COUNT > 0` and project inbox count >= limit:
     ```
     event: "quota_inbox_exceeded"
     project, inbox_count, limit
     ```

### Configuration Reference

| Env Var | Default | Worker |
|---|---|---|
| `FILE_RESERVATIONS_CLEANUP_ENABLED` | `false` | cleanup |
| `FILE_RESERVATIONS_CLEANUP_INTERVAL_SECONDS` | `60` | cleanup |
| `FILE_RESERVATION_INACTIVITY_SECONDS` | `1800` | cleanup (stale heuristics) |
| `FILE_RESERVATION_ACTIVITY_GRACE_SECONDS` | `900` | cleanup (stale heuristics) |
| `ACK_TTL_ENABLED` | `false` | ack_ttl |
| `ACK_TTL_SECONDS` | `1800` | ack_ttl |
| `ACK_TTL_SCAN_INTERVAL_SECONDS` | `60` | ack_ttl |
| `ACK_ESCALATION_ENABLED` | `false` | ack_ttl |
| `ACK_ESCALATION_MODE` | `log` | ack_ttl (`log` or `file_reservation`) |
| `ACK_ESCALATION_CLAIM_TTL_SECONDS` | `3600` | ack_ttl |
| `ACK_ESCALATION_CLAIM_EXCLUSIVE` | `false` | ack_ttl |
| `ACK_ESCALATION_CLAIM_HOLDER_NAME` | `""` | ack_ttl (empty = use recipient) |
| `TOOL_METRICS_EMIT_ENABLED` | `false` | tool_metrics |
| `TOOL_METRICS_EMIT_INTERVAL_SECONDS` | `60` | tool_metrics |
| `RETENTION_REPORT_ENABLED` | `false` | retention_quota |
| `RETENTION_REPORT_INTERVAL_SECONDS` | `3600` | retention_quota |
| `RETENTION_MAX_AGE_DAYS` | `180` | retention_quota |
| `RETENTION_IGNORE_PROJECT_PATTERNS` | `demo,test*,...` | retention_quota |
| `QUOTA_ENABLED` | `false` | retention_quota |
| `QUOTA_ATTACHMENTS_LIMIT_BYTES` | `0` | retention_quota (0 = disabled) |
| `QUOTA_INBOX_LIMIT_COUNT` | `0` | retention_quota (0 = disabled) |

### Failure Handling Expectations (Rust Port)

1. **Never crash the server.** Each worker loop iteration is catch-all safe.
2. **Per-project isolation.** One project's error in cleanup must not skip other projects.
3. **Graceful degradation.** If Rich is unavailable, fall back to plain print. If structlog unavailable, skip logging.
4. **Shutdown responsiveness.** Workers must check for cancellation and exit promptly.
5. **Sleep floor.** Tool metrics has `max(5, interval)`, retention has `max(60, interval)` — enforce these minimums.
6. **Timezone normalization.** SQLite may return naive datetimes; always normalize to UTC before arithmetic.
7. **Idempotent escalation.** Multiple scan passes may see the same overdue message — the file reservation INSERT uses `INSERT INTO` (not `INSERT OR IGNORE`), so duplicate reservations can be created. The Rust port should consider deduplication (e.g., check if a reservation with `reason='ack-overdue'` already exists for that path pattern).

### Test Coverage Reference

Source: `tests/test_http_workers_and_options.py`

| Test | What it validates |
|---|---|
| `test_http_ack_ttl_worker_log_mode` | ACK_TTL_SECONDS=0, scan runs, no crash, health check OK |
| `test_http_ack_ttl_worker_file_reservation_escalation` | Escalation creates file_reservation artifact readable via resource |
| `test_http_request_logging_and_cors_headers` | CORS preflight 200/204, request logging enabled |

---

## Concurrency Model

### Database
- SQLite WAL for concurrent readers + single writer
- Exponential backoff with jitter on lock contention
- Circuit breaker: 5 failures → open for 30s
- Connection pooling: 3 base + 4 overflow, recycle every 30min

### Git Archive
- Per-project advisory locks (`.archive.lock` via `filelock.SoftFileLock`)
- Lock metadata: `.archive.lock.owner.json`
- Commit queue with batching (max 10, max wait 50ms)
- Stale lock cleanup on startup

---

## Error Types

```
CAPABILITY_DENIED, NOT_FOUND, INVALID_ARGUMENT, TYPE_ERROR, MISSING_FIELD,
DATABASE_POOL_EXHAUSTED, TIMEOUT, GIT_INDEX_LOCK, RESOURCE_EXHAUSTED,
CONTACT_REQUIRED, CONTACT_BLOCKED, OS_ERROR, DATABASE_ERROR, RESOURCE_BUSY,
PERMISSION_ERROR, CONNECTION_ERROR, UNHANDLED_EXCEPTION
```

---

## TOON Output Format (Token-Optimized Output Notation)

Source: `app.py` lines 888-1178.

### Overview

TOON is an optional output encoding that compresses JSON tool/resource responses via an external encoder binary (`tru`). All TOON-formatted responses are wrapped in an **envelope** with metadata. When encoding fails, the system falls back gracefully to a JSON envelope with error details.

### Envelope Schema

**Successful TOON encoding:**
```json
{
  "format": "toon",
  "data": "<TOON-encoded-string>",
  "meta": {
    "requested": "toon",
    "source": "param|default|implicit",
    "encoder": "tru|<path-to-encoder>",
    "toon_stats": {
      "json_tokens": 123,
      "toon_tokens": 45,
      "saved_tokens": 78,
      "saved_percent": 63.4
    },
    "toon_stats_raw": null
  }
}
```

**Fallback (encoding failed):**
```json
{
  "format": "json",
  "data": { /* original JSON payload */ },
  "meta": {
    "requested": "toon",
    "source": "param|default|implicit",
    "toon_error": "<error description>"
  }
}
```

Required keys: `format`, `data`, `meta`. `meta` always has `requested` and `source`. On success, `meta.encoder` is present. On failure, `meta.toon_error` is present.

### Format Resolution

Resolution order for determining output format:

1. **Explicit parameter** (`format` tool arg or `?format=toon` query param):
   - Normalize via aliases, check validity
   - If valid: `source="param"`
   - If invalid: raise `ValueError`

2. **Config defaults** (if no explicit parameter):
   - Check `MCP_AGENT_MAIL_OUTPUT_FORMAT` env var first
   - Then check `TOON_DEFAULT_FORMAT` env var
   - If valid: `source="default"`

3. **Implicit fallback** (no parameter, no default):
   - Return `"json"` with `source="implicit"`, `requested=null`

**Auto-values** (treated as None/use defaults): `""`, `"auto"`, `"default"`, `"none"`, `"null"`.

**MIME type aliases:**

| Input | Normalized |
|-------|-----------|
| `application/json` | `json` |
| `text/json` | `json` |
| `application/toon` | `toon` |
| `text/toon` | `toon` |

### Encoder Selection

Precedence for the TOON encoder binary:

1. `TOON_TRU_BIN` env var (highest priority)
2. `TOON_BIN` env var
3. Hardcoded default `"tru"`

The value is split via `shlex.split()` (falls back to `[raw]` on parse error), then validated by `_looks_like_toon_rust_encoder()`.

### Encoder Validation

The `_looks_like_toon_rust_encoder(exe)` function (cached via LRU, max 32):

1. Extract basename (handle both `/` and `\` separators)
2. **Reject immediately** if basename is `toon` or `toon.exe` (Node.js CLI protection)
3. Run `exe --help`: if output contains `"reference implementation in rust"` (case-insensitive) → accept
4. Run `exe --version`: if output starts with `"tru "` or `"toon_rust "` (case-insensitive) → accept
5. Otherwise → reject

If validation fails: `ValueError` with message `"TOON_BIN resolved to {exe!r}, which does not look like toon_rust"`.

### Encoding Execution

Command: `[encoder, "--encode"]` + optional `"--stats"` if `TOON_STATS` enabled.

Input: JSON payload via stdin (text mode).
Output: TOON text from stdout, stats from stderr.
Non-blocking: run in `asyncio.to_thread` for async tool handlers.

### Fallback Triggers

In order of checking:
1. JSON serialization of payload fails → `toon_error: "json serialization failed: {e}"`
2. Encoder config invalid (validation fails) → `toon_error: "{ValueError message}"`
3. Encoder not found (`FileNotFoundError`) → `toon_error: "TOON encoder not found: {e}"`
4. OS error running encoder → `toon_error: "TOON encoder failed: {e}"`
5. Non-zero exit code → `toon_error: "TOON encoder exited with {code}"` + `toon_stderr: "<truncated stderr>"`

### Statistics Parsing

When `TOON_STATS=true` (env var), encoder runs with `--stats` flag. Stderr is parsed with two regexes:

**Tokens regex:** `Token estimates:\s*~(\d+)\s*\(JSON\)\s*(?:->|→)\s*~(\d+)\s*\(TOON\)`
**Saved regex:** `Saved\s*~(\d+)\s*tokens\s*\((-?\d+(?:\.\d+)?)%\)`

Expected stderr format:
```
Token estimates: ~10 (JSON) -> ~5 (TOON)
Saved ~5 tokens (-50.0%)
```

Parsed result: `{json_tokens, toon_tokens, saved_tokens, saved_percent}` or `None`.

If stats enabled but parsing fails, `meta.toon_stats_raw` contains truncated stderr (max 2000 chars).

### Tool Format Application

When a tool is called with a `format` parameter (or default format is configured):
1. Resolve format decision
2. If not TOON, return result unchanged
3. Extract structured payload from result
4. Encode payload via subprocess in thread pool
5. Return envelope (success or fallback)

### Resource Format Application

Resources accept `format` as a query parameter: `resource://inbox/{agent}?format=toon`.

All resource handlers pass `format_value` through `_apply_resource_output_format()` which:
1. Resolves format decision
2. If not TOON, returns payload unchanged
3. Encodes payload synchronously (resources are already sync)
4. Returns envelope (success or fallback)

### Environment Variables

| Variable | Purpose | Default |
|----------|---------|---------|
| `MCP_AGENT_MAIL_OUTPUT_FORMAT` | Default format for all tools/resources | (unset) |
| `TOON_DEFAULT_FORMAT` | Default format (lower precedence) | (unset) |
| `TOON_TRU_BIN` | Encoder binary path (highest priority) | (unset) |
| `TOON_BIN` | Encoder binary path (fallback) | (unset) |
| `TOON_STATS` | Enable stats gathering | `"false"` |

---

## Instrumentation / Query Tracking

### Overview

Optional per-tool query instrumentation that captures SQL statistics (total queries, time, per-table breakdown, slow queries) for each tool invocation. Entirely opt-in via environment variables.

### Environment Variables

| Variable | Default | Type | Description |
|----------|---------|------|-------------|
| `INSTRUMENTATION_ENABLED` | `"false"` | bool | Master switch for query tracking |
| `INSTRUMENTATION_SLOW_QUERY_MS` | `"250"` | int | Threshold for slow query capture (ms) |
| `TOOLS_LOG_ENABLED` | `"true"` | bool | Enable rich logger integration |

### QueryTracker

A per-invocation tracker that accumulates statistics across all SQL queries during a single tool call.

#### Fields

```
QueryTracker:
  total: int               # Total number of SQL queries executed
  total_time_ms: float     # Sum of all query durations, rounded to 2 decimal places
  per_table: dict[str,int] # Query count per table, sorted by (-count, name)
  slow_query_ms: float?    # Threshold for slow queries (None if disabled)
  slow_queries: list       # Captured slow queries (max 50)
```

#### Slow Query Capture

- **Threshold**: `settings.instrumentation_slow_query_ms` (default 250ms)
- **Limit**: 50 queries per tool invocation (`_SLOW_QUERY_LIMIT = 50`)
- **Each slow query records**:
  - `table`: extracted table name (or None if extraction fails)
  - `duration_ms`: query time rounded to 2 decimal places

#### `to_dict()` Output Shape

```json
{
  "total": 12,
  "total_time_ms": 45.67,
  "per_table": {"messages": 5, "agents": 4, "projects": 3},
  "slow_query_ms": 250.0,
  "slow_queries": [
    {"table": "messages", "duration_ms": 312.45},
    {"table": "agents", "duration_ms": 280.10}
  ]
}
```

**Sorting rules for `per_table`**: sorted by count descending, then table name ascending (alphabetical tiebreaker).

**Rounding rules**:
- `total_time_ms`: `round(value, 2)` — 2 decimal places
- `slow_queries[].duration_ms`: `round(value, 2)` — 2 decimal places
- `total`: integer, no rounding
- `slow_query_ms`: stored as-is (float from config), no rounding

### SQL Table Name Extraction

Three regex patterns tried in order (first match wins):

1. **INSERT INTO**: `\binsert\s+into\s+([\w\.\"`\[\]]+)` (case-insensitive)
2. **UPDATE**: `\bupdate\s+([\w\.\"`\[\]]+)` (case-insensitive)
3. **FROM**: `\bfrom\s+([\w\.\"`\[\]]+)` (case-insensitive)

After matching, the raw table name is cleaned:
1. Strip leading/trailing whitespace
2. If contains `.`, take only the last segment (strip schema prefix)
3. Strip surrounding quote characters: `` ` `` `"` `[` `]`

**Examples**:
- `SELECT * FROM messages WHERE id=1` → `"messages"`
- `INSERT INTO "public"."agents" (name) VALUES (?)` → `"agents"`
- `UPDATE [dbo].[projects] SET name=?` → `"projects"`
- `SELECT 1` → `None` (no FROM/INSERT/UPDATE clause)

### Tool Wrapper Integration

The query tracker is wired into the tool handler wrapper in `app.py`:

**Start tracking** (before tool execution):
```
if no active tracker AND instrumentation_enabled:
    create new QueryTracker(slow_ms=settings.instrumentation_slow_query_ms)
    set as active via context variable
```

**Collect stats** (after tool execution):
```
if tracker exists:
    query_stats = tracker.to_dict()
```

**Emit log** (only when stats exist AND instrumentation enabled):
```
logger.info("tool_query_stats", extra={
    "tool": tool_name,          # str: tool function name
    "project": project_value,   # str?: resolved project slug
    "agent": agent_value,       # str?: resolved agent name
    "queries": total,           # int: total query count
    "query_time_ms": time_ms,   # float: total time (rounded)
    "per_table": per_table,     # dict: sorted table breakdown
    "slow_query_ms": threshold  # float?: slow query threshold
})
```

**Log event name**: `"tool_query_stats"`

**Cleanup** (always, in finally block):
```
if tracker_token exists:
    reset context variable
```

### Rich Logger Presentation

When query stats are available, the rich logger renders a panel with:

1. **Table**: "DB Query Breakdown" showing top 5 tables by query count
2. **Slow queries section**: if any slow queries, shows "Slow queries (>= Nms)" header followed by top 5 slow queries with `table: Nms` format

### Interaction with DB Hooks

The tracking is powered by SQLAlchemy event hooks (legacy Python uses `before_cursor_execute` / `after_cursor_execute`). In Rust:

- Hook into sqlmodel_rust query execution (or wrap query calls)
- Use `std::time::Instant` for timing (not wall-clock)
- Context variable equivalent: thread-local or task-local storage
- Hook installation is idempotent (global flag prevents double-install)

### Config Fields in Rust

```rust
// In Config struct
pub instrumentation_enabled: bool,        // default: false
pub instrumentation_slow_query_ms: u64,   // default: 250
pub tools_log_enabled: bool,              // default: true
```

### Test Vectors

See `crates/mcp-agent-mail-db/tests/fixtures/instrumentation/` for:
- `table_extraction.json`: SQL statement → expected table name
- `tracker_aggregation.json`: sequence of queries → expected to_dict() output

---

*Spec extracted by FuchsiaForge | 2026-02-04*
*Updated by IndigoCreek | 2026-02-04*
*Share/Export spec added by CoralBadger | 2026-02-05*
*HTTP Background Workers spec added by CoralBadger | 2026-02-05*
*TOON Output Format spec added by CoralBadger | 2026-02-05*
*Instrumentation spec added by CoralBadger | 2026-02-05*

---

## Notifications (Signals)

### Overview

Optional push-notification system using local signal files. When enabled, sending a message touches a `.signal` file that agents can watch via inotify/FSEvents/kqueue for zero-poll notification. Entirely opt-in, designed for local multi-agent development.

### Environment Variables

| Variable | Default | Type | Description |
|----------|---------|------|-------------|
| `NOTIFICATIONS_ENABLED` | `"false"` | bool | Master switch for signal file emission |
| `NOTIFICATIONS_SIGNALS_DIR` | `"~/.mcp_agent_mail/signals"` | str | Root directory for signal files |
| `NOTIFICATIONS_INCLUDE_METADATA` | `"true"` | bool | Include message metadata in signal JSON |
| `NOTIFICATIONS_DEBOUNCE_MS` | `"100"` | int | Debounce window in milliseconds |

### NotificationSettings

```
NotificationSettings:
  enabled: bool           # Master switch
  signals_dir: str        # Root directory (supports ~ expansion)
  include_metadata: bool  # Include message fields in signal JSON
  debounce_ms: int        # Debounce window in ms (0 = always emit)
```

### Signal File Layout

```
{signals_dir}/
└── projects/
    ├── {project_slug}/
    │   └── agents/
    │       ├── {agent_name1}.signal    <- JSON file
    │       └── {agent_name2}.signal    <- JSON file
    └── {another_project}/
        └── agents/
            └── ...
```

Path formula: `{signals_dir}/projects/{project_slug}/agents/{agent_name}.signal`

The `signals_dir` path is expanded via `expanduser()` and `resolve()` (follow symlinks).

### Signal File JSON Payload

**With metadata** (`include_metadata=true` AND `message_metadata` provided):
```json
{
  "timestamp": "2024-01-01T12:00:00+00:00",
  "project": "test_project",
  "agent": "TestAgent",
  "message": {
    "id": 123,
    "from": "SenderAgent",
    "subject": "Hello World",
    "importance": "high"
  }
}
```

**Without metadata** (`include_metadata=false` OR no `message_metadata`):
```json
{
  "timestamp": "2024-01-01T12:00:00+00:00",
  "project": "test_project",
  "agent": "TestAgent"
}
```

**Timestamp format**: ISO-8601 with timezone, from `datetime.now(timezone.utc).isoformat()`.

**Message metadata fields** (all from the `message_metadata` dict):
- `id`: message ID (int or null if absent)
- `from`: sender name (str or null if absent)
- `subject`: message subject (str or null if absent)
- `importance`: message importance (str, defaults to `"normal"` if absent)

### Debounce Semantics

- **State**: In-memory dict mapping `(project_slug, agent_name)` → last emission timestamp (ms)
- **Time source**: `time.time() * 1000` (wall clock milliseconds)
- **Check**: `now_ms - last_signal < debounce_ms` → skip (return false)
- **Update**: On successful emission, record `now_ms` for the debounce key
- **Reset**: In tests, clear the debounce dict with `_SIGNAL_DEBOUNCE.clear()`
- **Key granularity**: Per-project per-agent (different agents in the same project debounce independently)

When `debounce_ms=0`, every emission passes the check (since `now - last >= 0`).

### emit_notification_signal

```
async fn emit_notification_signal(
    settings, project_slug, agent_name, message_metadata
) -> bool
```

**Flow**:
1. If `!settings.notifications.enabled` → return `false`
2. Debounce check: if `now_ms - last_signal_ms < debounce_ms` → return `false`
3. Update debounce state: `debounce[(project, agent)] = now_ms`
4. Build signal JSON payload (see above)
5. Create parent directories (`mkdir -p`)
6. Write signal file atomically (overwrite existing)
7. Return `true` on success, `false` on any exception (best-effort)

### clear_notification_signal

```
async fn clear_notification_signal(
    settings, project_slug, agent_name
) -> bool
```

**Flow**:
1. If `!settings.notifications.enabled` → return `false`
2. Compute signal path
3. If file exists, delete it → return `true`
4. If file doesn't exist → return `false`
5. On any exception → return `false` (best-effort)

### list_pending_signals

```
fn list_pending_signals(settings, project_slug: Option) -> Vec<dict>
```

**Flow**:
1. If `!settings.notifications.enabled` → return `[]`
2. If `signals_dir` doesn't exist → return `[]`
3. If `projects/` subdir doesn't exist → return `[]`
4. If `project_slug` provided, iterate only that project dir; otherwise iterate all project dirs
5. For each `agents/*.signal` file:
   - Parse JSON → append to results
   - On parse failure → append `{"project": dir_name, "agent": stem, "error": "Failed to parse signal file"}`
6. Return all results

**Note**: This function is synchronous (not async) in the Python implementation.

### Tool Wrapper Integration

**send_message** (after message is stored in DB + archive):
```
if settings.notifications.enabled:
    message_meta = {id, from: sender.name, subject, importance}
    for agent in to_agents + cc_agents:    # NOT bcc
        suppress(Exception):
            await emit_notification_signal(settings, project.slug, agent.name, message_meta)
```

Key: BCC recipients do NOT receive signals (blind copies shouldn't trigger visible notifications).

**fetch_inbox** (after successful inbox retrieval):
```
if settings.notifications.enabled:
    suppress(Exception):
        await clear_notification_signal(settings, project.slug, agent.name)
```

### Error Handling

All three functions are best-effort:
- `emit_notification_signal`: wraps file write in try/except, returns `false` on failure
- `clear_notification_signal`: wraps file delete in try/except, returns `false` on failure
- `list_pending_signals`: wraps individual file reads in try/except, includes error entries
- Integration in `send_message` and `fetch_inbox`: wrapped in `suppress(Exception)`

No notification failure should ever propagate up to cause a tool invocation failure.

### Config Fields in Rust

```rust
pub struct NotificationSettings {
    pub enabled: bool,              // default: false
    pub signals_dir: String,        // default: "~/.mcp_agent_mail/signals"
    pub include_metadata: bool,     // default: true
    pub debounce_ms: u64,           // default: 100
}
```

### Test Vectors

See `crates/mcp-agent-mail-storage/tests/fixtures/notifications/` for:
- `signal_payloads.json`: expected signal JSON for metadata on/off cases
- `debounce_scenarios.json`: sequence of emissions with expected outcomes
- `list_scenarios.json`: directory layouts with expected list results

*Notifications spec added by CoralBadger | 2026-02-05*
