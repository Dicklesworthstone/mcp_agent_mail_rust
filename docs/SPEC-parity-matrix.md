# Spec: MCP-to-CLI Parity Target Matrix

**Bead:** br-21gj.1.4
**Depends on:** ADR-001 (br-21gj.1.1)
**Date:** 2026-02-08

## Scope

Maps every MCP tool to its CLI equivalent (or gap), establishing the
implementation target for the CLI-mode command surface.

## Parity Labels

| Label | Meaning |
|-------|---------|
| **Exact** | CLI command exposes identical parameters and semantics |
| **Partial** | CLI command covers the common case; edge-case params omitted |
| **N/A** | Intentionally out-of-scope for CLI (MCP-transport-specific) |
| **Gap** | No CLI equivalent exists yet; should be implemented |

## Matrix

### Infrastructure Cluster

| MCP Tool | CLI Command | Parity | Notes |
|----------|-------------|--------|-------|
| `health_check` | `doctor check` | Partial | Doctor does more; health_check is a subset |
| `ensure_project` | `products ensure` | Exact | |
| `install_precommit_guard` | `guard install` | Exact | |
| `uninstall_precommit_guard` | `guard uninstall` | Exact | |

### Identity Cluster

| MCP Tool | CLI Command | Parity | Notes |
|----------|-------------|--------|-------|
| `register_agent` | -- | Gap | Add `agents register` command |
| `create_agent_identity` | -- | Gap | Add `agents create` command |

### Messaging Cluster

| MCP Tool | CLI Command | Parity | Notes |
|----------|-------------|--------|-------|
| `send_message` | -- | Gap | Add `mail send` command |
| `reply_message` | -- | Gap | Add `mail reply` command |
| `fetch_inbox` | `products inbox` | Partial | Product-scoped only; add `mail inbox` |
| `mark_message_read` | -- | Gap | Add `mail read` command |
| `acknowledge_message` | -- | Gap | Add `mail ack` command |

### Contact Cluster

| MCP Tool | CLI Command | Parity | Notes |
|----------|-------------|--------|-------|
| `request_contact` | -- | Gap | Add `contacts request` command |
| `respond_contact` | -- | Gap | Add `contacts respond` command |
| `list_contacts` | -- | Gap | Add `contacts list` command |
| `set_contact_policy` | -- | Gap | Add `contacts policy` command |

### File Reservations Cluster

| MCP Tool | CLI Command | Parity | Notes |
|----------|-------------|--------|-------|
| `file_reservation_paths` | -- | Gap | Add `reservations acquire` command |
| `release_file_reservations` | -- | Gap | Add `reservations release` command |
| `renew_file_reservations` | -- | Gap | Add `reservations renew` command |
| `force_release_file_reservation` | -- | Gap | Add `reservations force-release` |
| -- | `file_reservations list` | N/A | CLI-only read view (already exists) |
| -- | `file_reservations active` | N/A | CLI-only read view (already exists) |
| -- | `file_reservations soon` | N/A | CLI-only read view (already exists) |

### Search Cluster

| MCP Tool | CLI Command | Parity | Notes |
|----------|-------------|--------|-------|
| `search_messages` | `products search` | Partial | Product-scoped; add `mail search` |
| `summarize_thread` | `products summarize-thread` | Partial | Product-scoped; add `mail summarize` |

### Workflow Macros Cluster

| MCP Tool | CLI Command | Parity | Notes |
|----------|-------------|--------|-------|
| `macro_start_session` | -- | N/A | Agent-workflow-specific; no CLI equivalent needed |
| `macro_prepare_thread` | -- | N/A | Agent-workflow-specific |
| `macro_file_reservation_cycle` | -- | N/A | Agent-workflow-specific |
| `macro_contact_handshake` | -- | N/A | Agent-workflow-specific |

### Product Bus Cluster

| MCP Tool | CLI Command | Parity | Notes |
|----------|-------------|--------|-------|
| `ensure_product` | `products ensure` | Exact | |
| `products_link` | `products link` | Exact | |
| `search_messages_product` | `products search` | Exact | |
| `fetch_inbox_product` | `products inbox` | Exact | |
| `summarize_thread_product` | `products summarize-thread` | Exact | |

### Build Slots Cluster

| MCP Tool | CLI Command | Parity | Notes |
|----------|-------------|--------|-------|
| `acquire_build_slot` | -- | Gap | Add `slots acquire` command |
| `renew_build_slot` | -- | Gap | Add `slots renew` command |
| `release_build_slot` | -- | Gap | Add `slots release` command |

## CLI-Only Commands (No MCP Equivalent Needed)

These commands are operator-specific and do not need MCP tool equivalents:

| CLI Command | Rationale |
|-------------|-----------|
| `serve-http` / `serve-stdio` | Server lifecycle, not agent operations |
| `lint` / `typecheck` | Development tooling |
| `migrate` | Schema management |
| `clear-and-reset-everything` | Destructive admin operation |
| `share *` | Export/backup workflow |
| `archive *` | Snapshot management |
| `guard status/check` | Read-only guard inspection |
| `doctor *` | Diagnostics and repair |
| `config *` | Configuration management |
| `docs *` | Documentation generation |

## Summary

| Category | Exact | Partial | Gap | N/A |
|----------|-------|---------|-----|-----|
| Infrastructure | 3 | 1 | 0 | 0 |
| Identity | 0 | 0 | 2 | 0 |
| Messaging | 0 | 1 | 4 | 0 |
| Contacts | 0 | 0 | 4 | 0 |
| File Reservations | 0 | 0 | 4 | 0 |
| Search | 0 | 2 | 0 | 0 |
| Macros | 0 | 0 | 0 | 4 |
| Product Bus | 5 | 0 | 0 | 0 |
| Build Slots | 0 | 0 | 3 | 0 |
| **Total** | **8** | **4** | **17** | **4** |

## Implementation Priority

Highest priority (agent-first workflows):

1. **Messaging** (`mail send/reply/inbox/read/ack`) — most frequently used
2. **Identity** (`agents register/create`) — needed for session bootstrap
3. **File Reservations** (`reservations acquire/release/renew/force-release`) — needed for safe multi-agent editing
4. **Contacts** (`contacts request/respond/list/policy`) — needed for agent coordination
5. **Build Slots** (`slots acquire/renew/release`) — needed for worktree workflows
6. **Search** (`mail search/summarize`) — useful but lower frequency

This priority order is reflected in the CLI command families in Track 4
(br-21gj.4.2 through br-21gj.4.7).
