# Plan to Port MCP Agent Mail to Rust

> Historical note (2026-04-18): This is an early port-planning artifact. Tool,
> resource, and screen counts below do not describe the current live product;
> use `README.md`, `AGENTS.md`, and `docs/DOC_SWEEP_AUDIT_2026-04-18.md` for
> current surface claims.

## Scope (Explicit)
- Goal: 100% feature and behavior parity with legacy Python `mcp_agent_mail`.
- Exclusions: None. If any exclusions arise, they must be added here explicitly with rationale.

## Non-Negotiable Dependencies (From User Requirements)
- MCP server and transport: `/dp/fastmcp_rust` (use as crate or pillage as needed).
- SQLite: `/dp/sqlmodel_rust` only (no `rusqlite` direct usage).
- Async + networking + general I/O: `/dp/asupersync` (no tokio).
- Console/TUI rendering: `/dp/frankentui` only.
- Beads integration: `/dp/beads_rust`.
- Coding-agent detection: `/dp/coding_agent_session_search`.

## Porting Phases
1. Phase 1: Bootstrap + Planning
- Create spec docs and architecture docs.
- Establish parity matrix and conformance plan.
- Decide crate layout and dependency wiring.

2. Phase 2: Deep Dive Extraction (Spec)
- Extract data models, schema, and invariants.
- Extract all CLI commands, flags, defaults, and outputs.
- Extract HTTP/MCP tool surface, resource URIs, and outputs.
- Extract storage layout, Git archive semantics, and lock rules.

3. Phase 3: Architecture Synthesis
- Design Rust modules aligned with fastmcp_rust and sqlmodel_rust.
- Define storage architecture with archive as source-of-truth and SQLite as index.
- Define conformance harness and benchmarking strategy.

4. Phase 4: Implementation
- Implement core storage + DB schema + migrations.
- Implement MCP server tools and resource layer.
- Implement CLI, guard, share/export, and diagnostics.
- Implement conformance + benchmarks.

5. Phase 5: Conformance & QA
- Run fixture-based conformance vs Python reference.
- Run benchmarks and compare to baselines.
- Update FEATURE_PARITY.md to 100%.

## Deliverables
- `EXISTING_MCP_AGENT_MAIL_STRUCTURE.md` (full spec).
- `PROPOSED_ARCHITECTURE.md` (Rust design and module layout).
- `FEATURE_PARITY.md` (tracking + evidence for each feature).
- `SYNC_STRATEGY.md` and `RECOVERY_RUNBOOK.md` (SQLite + Git archive sync plan).
- Conformance harness and fixtures with automated runner.
- Benchmarks that reuse conformance cases.

## Conformance Strategy (Baseline)
- Use Python reference runner to generate fixtures for:
- MCP tool outputs and error JSON.
- Resource endpoints (`resource://...`).
- CLI outputs for common commands.
- Guard outputs and denial JSON.
- Store fixtures in `tests/conformance/fixtures/` with a version header.
- Compare Rust outputs to fixtures in tests and CI.

## Benchmark Strategy (Baseline)
- Use conformance cases as benchmark inputs.
- Measure command latency, search performance, and archive write throughput.
- Record baselines and track regressions.

## Risks / Constraints
- Tooling and async stack must avoid tokio.
- SQLite must go through sqlmodel_rust.
- No script-based code modifications inside this repo.
- Multi-agent edits may happen concurrently; avoid clobbering.

## Immediate Next Steps
- Build spec skeleton for models, CLI, MCP tools, resources, and storage layout.
- Explore legacy Python modules and extract core invariants.
- Inspect reference crates and map to Rust architecture.

---

## Detailed Implementation Phases

### Phase 1: Foundation (Core Types & Storage Schema)

**Tasks:**
1. Create workspace Cargo.toml with all crates
2. Define core models in `mcp-agent-mail-core`:
   - `Project`, `Agent`, `Message`, `MessageRecipient`
   - `FileReservation`, `AgentLink`, `Product`, `ProductProjectLink`
   - `ProjectSiblingSuggestion`
3. Define SQLite schema in `mcp-agent-mail-storage`:
   - Tables matching Python exactly
   - FTS5 virtual table for message search
   - All indexes from Python
4. Implement basic CRUD via sqlmodel_rust
5. Configuration system from environment variables

**Exit Criteria:**
- [ ] `cargo check` passes
- [ ] Unit tests for model creation/query
- [ ] Schema matches Python byte-for-byte

### Phase 2: MCP Tools (35 tools)

**Identity (5 tools):** `health_check`, `ensure_project`, `register_agent`, `create_agent_identity`, `whois`

**Messaging (5 tools):** `send_message`, `reply_message`, `fetch_inbox`, `mark_message_read`, `acknowledge_message`

**Contact (4 tools):** `request_contact`, `respond_contact`, `list_contacts`, `set_contact_policy`

**File Reservations (6 tools):** `file_reservation_paths`, `release_file_reservations`, `renew_file_reservations`, `force_release_file_reservation`, `install_precommit_guard`, `uninstall_precommit_guard`

**Search (2 tools):** `search_messages`, `summarize_thread`

**Macros (4 tools):** `macro_start_session`, `macro_prepare_thread`, `macro_file_reservation_cycle`, `macro_contact_handshake`

**Product (5 tools):** `ensure_product`, `products_link`, `search_messages_product`, `fetch_inbox_product`, `summarize_thread_product`

**Build Slots (3 tools):** `acquire_build_slot`, `renew_build_slot`, `release_build_slot`

**Implementation via fastmcp_rust:**
```rust
#[tool(description = "Ensure project exists")]
async fn ensure_project(ctx: &McpContext, human_key: String, identity_mode: Option<String>) -> McpResult<ProjectResult> {
    ctx.checkpoint()?;
    // Implementation...
}
```

### Phase 3: MCP Resources (20+ resources)

All resources implemented via `#[resource]` macro from fastmcp_rust.

### Phase 4: Git Archive Layer

**Components:**
- Archive structure creation (`agents/`, `messages/`, `attachments/`, `file_reservations/`)
- Message persistence (canonical + inbox/outbox copies)
- Attachment handling (WebP conversion, inline/file policy)
- Commit queue with batching
- Lock management (`.archive.lock` + metadata)
- Stale lock cleanup

**Uses:** git2-rs for Git operations

### Phase 5: HTTP Transport & Server

**Components:**
- HTTP transport via fastmcp_rust
- Bearer token auth middleware
- JWT auth middleware (HMAC + JWKS)
- RBAC middleware (reader/writer roles)
- Token bucket rate limiting (memory backend)
- CORS middleware

### Phase 6: CLI

**Commands:**
- `mcp-agent-mail` (main server)
- `guard install/uninstall`
- `file_reservations list/conflicts`
- `acks pending/overdue`
- `share export/bundle`
- `archive create/restore`
- `mail inbox/send`
- `projects list/gc`
- `products ensure/link`
- `doctor check/repair`

**Uses:** clap for CLI parsing, frankentui for output

### Phase 7: Conformance & Benchmarks

**Fixture Generation:**
- Python script to capture tool outputs, resource responses, CLI outputs
- Store in `tests/conformance/fixtures/python_outputs.json`

**Conformance Tests:**
- Load fixtures, call Rust implementation, compare outputs
- Normalization for timestamps, IDs

**Benchmarks:**
- `fetch_inbox` (target: < 5ms for 20 msgs)
- `send_message` (target: < 30ms including Git)
- `search_messages` (target: < 10ms)
- Startup time (target: < 50ms)

---

## Agent Work Division

| Agent | Assigned Area |
|-------|--------------|
| **FuchsiaForge** | Architecture, planning, core models, MCP tools framework, messaging tools, search tools, macro tools |
| **CrimsonGorge** | SQLite storage layer, Git archive integration, file reservation tools, contact tools, guard tools, conformance tests, benchmarks |

---

## Crate Layout Reference

```
mcp_agent_mail_rust/
├── Cargo.toml (workspace)
├── crates/
│   ├── mcp-agent-mail/           # Main binary
│   ├── mcp-agent-mail-core/      # Core types, config, error
│   ├── mcp-agent-mail-storage/   # SQLite + Git archive
│   ├── mcp-agent-mail-tools/     # MCP tools implementation
│   ├── mcp-agent-mail-server/    # HTTP transport, middleware
│   └── mcp-agent-mail-cli/       # CLI commands
├── tests/
│   ├── conformance/
│   │   ├── fixtures/
│   │   ├── python_reference/
│   │   └── conformance_test.rs
│   └── e2e/
└── benches/
```

---

*Plan updated by FuchsiaForge | 2026-02-04*
