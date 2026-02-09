# Feature Parity: MCP Agent Mail (Python -> Rust)

## Status Legend
- Not Started
- In Progress
- Implemented
- Verified (Conformance)
- Verified (Tests)

## Core Server Features
| Feature | Status | Evidence |
| --- | --- | --- |
| FastMCP server (stdio transport) | Verified (Tests) | `crates/mcp-agent-mail-server/src/lib.rs`, `tests/e2e/test_stdio.sh` (10 E2E assertions: initialize, tools/list, tool call, invalid JSON, clean exit) |
| FastMCP server (HTTP) | Verified (Tests) | `crates/mcp-agent-mail-server/src/lib.rs`, `scripts/e2e_http.sh` (47 E2E assertions), `tests/e2e/test_http.sh` |
| Tool registry + tool clusters | Verified (Conformance) | `crates/mcp-agent-mail-server/src/lib.rs` |
| Resource URI system (`resource://...`) | Verified (Conformance) | `crates/mcp-agent-mail-tools/src/resources.rs` |
| Static projects resource query handling (`format`, `limit`, `contains`) | Verified (Tests) | `crates/mcp-agent-mail-tools/src/resources.rs` (`projects_list_query`, query option parser + unit tests in `query_param_tests`) |
| FastMCP resource query routing edge cases (`resource://projects?...`) | Verified (Tests) | `crates/mcp-agent-mail-conformance/tests/conformance.rs` (`resource_query_router_projects_limit_and_contains_are_honored`, `resource_query_router_projects_invalid_query_values_surface_errors`) |
| Tool filtering profiles (full/core/minimal/messaging/custom) | Verified (Tests) | `crates/mcp-agent-mail-core/src/config.rs`, `crates/mcp-agent-mail-tools/src/lib.rs`, `crates/mcp-agent-mail-server/src/lib.rs`, `crates/mcp-agent-mail-conformance/tests/conformance.rs` |
| Tool metrics snapshot | Verified (Conformance) | `crates/mcp-agent-mail-tools/src/resources.rs` |
| Recent tool usage tracking | Verified (Conformance) | `crates/mcp-agent-mail-tools/src/resources.rs` |

## HTTP Security & Rate Limiting
| Feature | Status | Evidence |
| --- | --- | --- |
| Bearer auth (`HTTP_BEARER_TOKEN`) + localhost bypass | Verified (Tests) | `crates/mcp-agent-mail-server/src/lib.rs` |
| JWT auth (HS256 secret + JWKS + issuer/audience + role claim) | Verified (Tests) | `crates/mcp-agent-mail-server/src/lib.rs` (jwt_* tests) |
| RBAC enforcement (reader/writer roles, readonly tools) | Verified (Tests) | `crates/mcp-agent-mail-server/src/lib.rs` |
| Rate limit identity prefers JWT `sub` | Verified (Tests) | `crates/mcp-agent-mail-server/src/lib.rs` |

## Identity & Projects
| Feature | Status | Evidence |
| --- | --- | --- |
| Ensure project (+ archive init) | Verified (Conformance) | `crates/mcp-agent-mail-tools/src/identity.rs` |
| Register agent (+ profile archive write) | Verified (Conformance) | `crates/mcp-agent-mail-tools/src/identity.rs` |
| Create agent identity (+ profile archive write) | Verified (Conformance) | `crates/mcp-agent-mail-tools/src/identity.rs` |
| Whois (+ recent commits from archive) | Verified (Conformance) | `crates/mcp-agent-mail-tools/src/identity.rs` |
| Agent contact policy | Verified (Conformance) | `crates/mcp-agent-mail-tools/src/contacts.rs` |
| Agent links (contact requests) | Verified (Conformance) | `crates/mcp-agent-mail-tools/src/contacts.rs` |
| Product bus (products + links) | Verified (Conformance) | `crates/mcp-agent-mail-tools/src/products.rs` |

## Messaging
| Feature | Status | Evidence |
| --- | --- | --- |
| Send message (+ archive write) | Verified (Conformance) | `crates/mcp-agent-mail-tools/src/messaging.rs` |
| Reply message (+ archive write) | Verified (Conformance) | `crates/mcp-agent-mail-tools/src/messaging.rs` |
| Fetch inbox | Verified (Conformance) | `crates/mcp-agent-mail-tools/src/messaging.rs` |
| Search messages (FTS: sanitization + LIKE fallback) | Verified (Tests) | `crates/mcp-agent-mail-db/src/queries.rs` (20 tests), `crates/mcp-agent-mail-tools/src/search.rs` |
| Summarize thread (heuristic + LLM refinement) | Verified (Tests) | `crates/mcp-agent-mail-tools/src/search.rs`, `crates/mcp-agent-mail-tools/src/llm.rs` (20 tests) |
| LLM integration (env bridge + model selection + completion + merge) | Verified (Tests) | `crates/mcp-agent-mail-tools/src/llm.rs` (20 tests) |
| Subject truncation (200 chars, UTF-8 safe) | Verified (Tests) | `crates/mcp-agent-mail-tools/src/messaging.rs` (char-boundary-safe truncation) |
| Acknowledge + read tracking | Verified (Conformance) | `crates/mcp-agent-mail-tools/src/messaging.rs` |
| Ack-required views | Verified (Conformance) | `crates/mcp-agent-mail-tools/src/resources.rs` |
| Attachment handling + WebP conversion | Verified (Tests) | `crates/mcp-agent-mail-storage/src/lib.rs` (7 tests), `crates/mcp-agent-mail-tools/src/messaging.rs` |

## File Reservations
| Feature | Status | Evidence |
| --- | --- | --- |
| Reserve paths (+ archive artifact write) | Verified (Conformance) | `crates/mcp-agent-mail-tools/src/reservations.rs` |
| Renew reservations | Verified (Conformance) | `crates/mcp-agent-mail-tools/src/reservations.rs` |
| Release reservations | Verified (Conformance) | `crates/mcp-agent-mail-tools/src/reservations.rs` |
| Force-release stale reservations | Verified (Tests) | `crates/mcp-agent-mail-tools/src/reservations.rs` (inactivity heuristics + notification), `tests/e2e/test_stdio.sh` (E2E: setup stale agent, force-release, verify response + notification + error path) |
| Reservation views | Verified (Conformance) | `crates/mcp-agent-mail-tools/src/resources.rs` |

## Build Slots
| Feature | Status | Evidence |
| --- | --- | --- |
| Acquire build slot | Verified (Conformance) | `crates/mcp-agent-mail-tools/src/build_slots.rs` |
| Renew build slot | Verified (Conformance) | `crates/mcp-agent-mail-tools/src/build_slots.rs` |
| Release build slot | Verified (Conformance) | `crates/mcp-agent-mail-tools/src/build_slots.rs` |

## Storage & Database
| Feature | Status | Evidence |
| --- | --- | --- |
| SQLite schema + migrations (idempotent init) | Verified (Tests) | `crates/mcp-agent-mail-db/src/schema.rs` |
| DB pool + WAL PRAGMAs | Verified (Tests) | `crates/mcp-agent-mail-db/src/pool.rs` |
| Core queries used by tools | Verified (Conformance) | `crates/mcp-agent-mail-db/src/queries.rs` |
| FTS indexing | Verified (Tests) | `crates/mcp-agent-mail-db/src/schema.rs` (triggers), `crates/mcp-agent-mail-db/src/queries.rs` (20 FTS search tests), `crates/mcp-agent-mail-tools/src/search.rs` |
| Git archive init + per-project repos | Verified (Tests) | `crates/mcp-agent-mail-storage/src/lib.rs` |
| Agent profile archive writes | Verified (Tests) | `crates/mcp-agent-mail-storage/src/lib.rs` |
| Message bundle write pipeline | Verified (Tests) | `crates/mcp-agent-mail-storage/src/lib.rs` |
| File reservation artifact writes | Verified (Tests) | `crates/mcp-agent-mail-storage/src/lib.rs` |
| Advisory file locks (stale detection) | Verified (Tests) | `crates/mcp-agent-mail-storage/src/lib.rs` |
| Commit queue with batching | Verified (Tests) | `crates/mcp-agent-mail-storage/src/lib.rs` |
| Git index.lock contention retry | Verified (Tests) | `crates/mcp-agent-mail-storage/src/lib.rs` |
| Stale lock healing (startup) | Verified (Tests) | `crates/mcp-agent-mail-storage/src/lib.rs` |
| Notification signals | Verified (Tests) | `crates/mcp-agent-mail-storage/src/lib.rs` |
| Recent commits (path-filtered) | Verified (Tests) | `crates/mcp-agent-mail-storage/src/lib.rs` |
| Lock status diagnostics | Verified (Tests) | `crates/mcp-agent-mail-storage/src/lib.rs` (4 tests: nonexistent root, empty root, finds locks, owner metadata) |
| Thread digest (append-only) | Verified (Tests) | `crates/mcp-agent-mail-storage/src/lib.rs` |
| Storage-to-tools wiring | Verified (Tests) | identity.rs, messaging.rs, reservations.rs, resources.rs |
| DB↔storage consistency integration (message + reservation pipelines) | Verified (Tests) | `crates/mcp-agent-mail-storage/src/lib.rs` (`db_and_storage_message_pipeline_consistent`, `db_and_storage_reservation_pipeline_consistent`) |
| Query tracking + slow queries | Verified (Tests) | `crates/mcp-agent-mail-db/src/tracking.rs` (11 tests, 40 fixture vectors), `crates/mcp-agent-mail-db/src/queries.rs` (TrackedConnection, all calls instrumented), `crates/mcp-agent-mail-server/src/lib.rs` (auto-enable) |

## CLI & Ops
| Feature | Status | Evidence |
| --- | --- | --- |
| CLI command parity | Verified (Tests) | `crates/mcp-agent-mail-cli/src/lib.rs` (40+ commands, 123+ tests, only `--interactive` deferred) |
| `mcp-agent-mail` binary unsupported commands fail fast with CLI guidance | Verified (Tests) | `crates/mcp-agent-mail/src/main.rs` (non-zero exit + guidance + regression test) |
| CLI output module (TTY-aware tables, JSON mode) | Verified (Tests) | `crates/mcp-agent-mail-cli/src/output.rs` (CliTable, json_or_table, 33 tests) |
| Config management (show-port, set-port) | Verified (Tests) | `crates/mcp-agent-mail-cli/src/lib.rs` (2 integration tests: show-port returns port, set-port writes env file) |
| list-projects (--json, --include-agents) | Verified (Tests) | `crates/mcp-agent-mail-cli/src/lib.rs` (integration test), `tests/cli_json_snapshots.rs` (JSON snapshot) |
| file_reservations (list/active/soon) | Verified (Tests) | `crates/mcp-agent-mail-cli/src/lib.rs` (3 integration tests: list, active, empty project) |
| acks (pending/remind/overdue) | Verified (Tests) | `crates/mcp-agent-mail-cli/src/lib.rs` (4 integration tests: pending, empty, overdue, remind) |
| list-acks | Verified (Tests) | `crates/mcp-agent-mail-cli/src/lib.rs` (2 integration tests: shows ack-required messages, empty for nonexistent) |
| mail status | Verified (Tests) | `crates/mcp-agent-mail-cli/src/lib.rs` (2 integration tests: shows counts, empty project) |
| migrate | Verified (Tests) | `crates/mcp-agent-mail-cli/src/lib.rs` (10 tests: output parity, idempotent, tables, FTS, negative cases) |
| clear-and-reset-everything | Verified (Tests) | `crates/mcp-agent-mail-cli/src/lib.rs` (2 integration tests: force+archive, force+no-archive) |
| lint / typecheck | Verified (Tests) | `crates/mcp-agent-mail-cli/src/lib.rs`, `scripts/e2e_cli.sh` (typecheck passes, lint runs clippy) |
| projects (mark-identity/discovery-init/adopt) | Verified (Tests) | `crates/mcp-agent-mail-cli/src/lib.rs`, `tests/integration_runs.rs` (5 tests: mark-identity, discovery-init, adopt dry-run, adopt apply, adopt cross-repo) |
| doctor check (--verbose, --json) | Verified (Tests) | `crates/mcp-agent-mail-cli/src/lib.rs` (integration test), `tests/cli_json_snapshots.rs` (JSON snapshot) |
| beads_rust integration (issue/task awareness) | Verified (Tests) | `crates/mcp-agent-mail-cli/src/lib.rs` (`beads_issue_awareness_counts`, `doctor check` `beads_issue_awareness` probe + integration test) |
| coding_agent_session_search integration (installed agent detection) | Verified (Tests) | `crates/mcp-agent-mail-core/src/agent_detect.rs`, `crates/mcp-agent-mail-core/tests/agent_detect_integration.rs`, `crates/mcp-agent-mail-cli/src/lib.rs` (`doctor check` `installed_agents` probe + integration test) |
| Guard install/uninstall + conflict detection | Verified (Tests) | `crates/mcp-agent-mail-guard/src/lib.rs`, `tests/e2e/test_guard.sh` (32 E2E assertions: install, check, uninstall, conflict detection, case-insensitive, advisory mode) |
| Guard status / guard check (Rust native) | Verified (Tests) | `crates/mcp-agent-mail-guard/src/lib.rs`, `tests/e2e/test_guard.sh` (status, check with stdin-nul, advisory mode) |
| Doctor repair/backups/restore | Verified (Tests) | `crates/mcp-agent-mail-cli/src/lib.rs` (repair, backups, restore commands; backups integration test), `tests/cli_json_snapshots.rs` (backups JSON snapshot) |
| Share export/update/verify/decrypt/preview | Verified (Tests) | `crates/mcp-agent-mail-share/src/` (8 modules, 62 tests), `crates/mcp-agent-mail-cli/src/lib.rs` |
| Archive save/list/restore | Verified (Tests) | `crates/mcp-agent-mail-cli/src/lib.rs` (roundtrip smoke test), `tests/cli_json_snapshots.rs` (archive list JSON snapshots), `tests/e2e/test_archive.sh` (16 E2E assertions) |
| Crates.io publish workflow (tag-gated, ordered workspace publish) | Implemented | `.github/workflows/publish.yml` |

## Conformance & Benchmarks
| Feature | Status | Evidence |
| --- | --- | --- |
| Python fixture generator | Verified (Tests) | `crates/mcp-agent-mail-conformance/tests/conformance/python_reference/generate_fixtures.py` |
| Rust conformance tests (34 tools) | Verified (Tests) | `crates/mcp-agent-mail-conformance/tests/conformance.rs` |
| All resources (33) | Verified (Conformance) | `crates/mcp-agent-mail-tools/src/resources.rs` |
| Tool filter conformance (6 profiles) | Verified (Tests) | `crates/mcp-agent-mail-conformance/tests/conformance.rs` |
| Notification signal tests (9 tests) | Verified (Tests) | `crates/mcp-agent-mail-storage/src/lib.rs` |
| Instrumentation fixture tests (40 vectors) | Verified (Tests) | `crates/mcp-agent-mail-db/src/tracking.rs` |
| Instrumentation lifecycle + integration | Verified (Tests) | `crates/mcp-agent-mail-db/src/tracking.rs` (20+ unit), `tests/workers.rs` (4 integration) |
| Benchmark suite | Verified (Tests) | `crates/mcp-agent-mail/benches/benchmarks.rs` |

## HTTP Server (Logging, Health, OTEL)
| Feature | Status | Evidence |
| --- | --- | --- |
| HTTP request logging (KV + JSON + panel) | Verified (Tests) | `crates/mcp-agent-mail-server/src/lib.rs` (37 unit tests: formatters, TTY/non-TTY panel, field derivation) |
| ExpectedErrorFilter (8 patterns + cause chain) | Verified (Tests) | `crates/mcp-agent-mail-server/src/lib.rs` (10 tests: each pattern, case-insensitive, level preservation, cause chain) |
| OTEL config no-op parity | Verified (Tests) | `crates/mcp-agent-mail-server/src/lib.rs`, `tests/http_logging.rs` (no spans/traces emitted) |
| Health endpoints (/health/liveness, /health/readiness) | Verified (Tests) | `crates/mcp-agent-mail-server/src/lib.rs` (27 unit tests: JSON payloads, content-type, 405, auth bypass) |
| Well-known endpoints (/.well-known/oauth-authorization-server) | Verified (Tests) | `crates/mcp-agent-mail-server/src/lib.rs` (both paths, auth required, 405) |
| HTTP logging config gating | Verified (Tests) | `tests/http_logging.rs` (10 integration tests: enable matrix, defaults) |
| Health endpoint integration | Verified (Tests) | `tests/health_endpoints.rs` (5 integration tests) |

## Background Workers
| Feature | Status | Evidence |
| --- | --- | --- |
| ACK TTL scan + escalation | Verified (Tests) | `crates/mcp-agent-mail-server/src/ack_ttl.rs` (3 unit), `tests/workers.rs` (4 integration) |
| Tool metrics emit | Verified (Tests) | `crates/mcp-agent-mail-server/src/tool_metrics.rs` (3 unit), `tests/workers.rs` (2 integration) |
| Retention/quota report | Verified (Tests) | `crates/mcp-agent-mail-server/src/retention.rs` (9 unit), `tests/workers.rs` (2 integration) |
| File reservations cleanup | Verified (Tests) | `crates/mcp-agent-mail-server/src/cleanup.rs` (unit), `tests/workers.rs` (config gating) |
