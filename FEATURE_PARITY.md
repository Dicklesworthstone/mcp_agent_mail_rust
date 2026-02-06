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
| FastMCP server (stdio transport) | Implemented | `crates/mcp-agent-mail-server/src/lib.rs` |
| FastMCP server (HTTP) | Implemented | `crates/mcp-agent-mail-server/src/lib.rs` |
| Tool registry + tool clusters | Verified (Conformance) | `crates/mcp-agent-mail-server/src/lib.rs` |
| Resource URI system (`resource://...`) | Verified (Conformance) | `crates/mcp-agent-mail-tools/src/resources.rs` |
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
| Subject truncation (200 chars) | Implemented | `crates/mcp-agent-mail-tools/src/messaging.rs` |
| Acknowledge + read tracking | Verified (Conformance) | `crates/mcp-agent-mail-tools/src/messaging.rs` |
| Ack-required views | Verified (Conformance) | `crates/mcp-agent-mail-tools/src/resources.rs` |
| Attachment handling + WebP conversion | Verified (Tests) | `crates/mcp-agent-mail-storage/src/lib.rs` (7 tests), `crates/mcp-agent-mail-tools/src/messaging.rs` |

## File Reservations
| Feature | Status | Evidence |
| --- | --- | --- |
| Reserve paths (+ archive artifact write) | Verified (Conformance) | `crates/mcp-agent-mail-tools/src/reservations.rs` |
| Renew reservations | Verified (Conformance) | `crates/mcp-agent-mail-tools/src/reservations.rs` |
| Release reservations | Verified (Conformance) | `crates/mcp-agent-mail-tools/src/reservations.rs` |
| Force-release stale reservations | Implemented | `crates/mcp-agent-mail-tools/src/reservations.rs` (inactivity heuristics + notification) |
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
| FTS indexing | Implemented | `crates/mcp-agent-mail-db/src/schema.rs` |
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
| Lock status diagnostics | Implemented | `crates/mcp-agent-mail-storage/src/lib.rs` |
| Thread digest (append-only) | Verified (Tests) | `crates/mcp-agent-mail-storage/src/lib.rs` |
| Storage-to-tools wiring | Verified (Tests) | identity.rs, messaging.rs, reservations.rs, resources.rs |
| Query tracking + slow queries | Verified (Tests) | `crates/mcp-agent-mail-db/src/tracking.rs` (11 tests, 40 fixture vectors), `crates/mcp-agent-mail-db/src/queries.rs` (TrackedConnection, all calls instrumented), `crates/mcp-agent-mail-server/src/lib.rs` (auto-enable) |

## CLI & Ops
| Feature | Status | Evidence |
| --- | --- | --- |
| CLI command parity | Verified (Tests) | `crates/mcp-agent-mail-cli/src/lib.rs` (40+ commands, 123+ tests, only `--interactive` deferred) |
| CLI output module (TTY-aware tables, JSON mode) | Verified (Tests) | `crates/mcp-agent-mail-cli/src/output.rs` (CliTable, json_or_table, 33 tests) |
| Config management (show-port, set-port) | Implemented | `crates/mcp-agent-mail-cli/src/lib.rs` |
| list-projects (--json, --include-agents) | Implemented | `crates/mcp-agent-mail-cli/src/lib.rs` |
| file_reservations (list/active/soon) | Implemented | `crates/mcp-agent-mail-cli/src/lib.rs` |
| acks (pending/remind/overdue) | Implemented | `crates/mcp-agent-mail-cli/src/lib.rs` |
| list-acks | Implemented | `crates/mcp-agent-mail-cli/src/lib.rs` |
| mail status | Implemented | `crates/mcp-agent-mail-cli/src/lib.rs` |
| migrate | Verified (Tests) | `crates/mcp-agent-mail-cli/src/lib.rs` (10 tests: output parity, idempotent, tables, FTS, negative cases) |
| clear-and-reset-everything | Implemented | `crates/mcp-agent-mail-cli/src/lib.rs` |
| lint / typecheck | Implemented | `crates/mcp-agent-mail-cli/src/lib.rs` |
| projects (mark-identity/discovery-init/adopt) | Implemented | `crates/mcp-agent-mail-cli/src/lib.rs` |
| doctor check (--verbose, --json) | Implemented | `crates/mcp-agent-mail-cli/src/lib.rs` |
| Guard install/uninstall + conflict detection | Implemented | `crates/mcp-agent-mail-guard/src/lib.rs` |
| Guard status / guard check (Rust native) | Implemented | `crates/mcp-agent-mail-guard/src/lib.rs` |
| Doctor repair/backups/restore | Implemented | `crates/mcp-agent-mail-cli/src/lib.rs` (repair, backups, restore commands) |
| Share export/update/verify/decrypt/preview | Verified (Tests) | `crates/mcp-agent-mail-share/src/` (8 modules, 62 tests), `crates/mcp-agent-mail-cli/src/lib.rs` |
| Archive save/list/restore | Implemented | `crates/mcp-agent-mail-cli/src/lib.rs` |

## Conformance & Benchmarks
| Feature | Status | Evidence |
| --- | --- | --- |
| Python fixture generator | Verified (Tests) | `crates/mcp-agent-mail-conformance/tests/conformance/python_reference/generate_fixtures.py` |
| Rust conformance tests (23 tools) | Verified (Tests) | `crates/mcp-agent-mail-conformance/tests/conformance.rs` |
| All resources (23+) | Verified (Conformance) | `crates/mcp-agent-mail-tools/src/resources.rs` |
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
