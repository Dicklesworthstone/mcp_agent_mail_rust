# mcp-agent-mail-conformance

Rust-side conformance harnesses for MCP Agent Mail.

This crate keeps the live Rust router honest against the legacy Python reference where parity is intentional, and it records the remaining Rust-native gaps so drift is explicit instead of accidental.

## What lives here

- Behavior parity fixtures in `tests/conformance/fixtures/python_reference.json`
- Regeneration artifacts in `tests/conformance/fixtures/scenario_catalog.json` and `tests/conformance/fixtures/python_reference_regen_report.md`
- Rust-native golden fixtures in `tests/conformance/fixtures/rust_native/`
- Tool description parity against `../../tests/conformance/fixtures/tool_descriptions.json`
- Resource description drift guards for shared resource templates
- Focused regression checks for tool filtering, error envelopes, and outage behavior
- Automation entrypoints in `../../scripts/regen_python_parity_fixtures.sh` and `.github/workflows/conformance-fixture-regen.yml`

## Current coverage (as of 2026-04-18)

- The live Rust router exposes 37 tools.
- 34 tools have Python behavior fixtures in `tests/conformance/fixtures/python_reference.json`.
- 3 tools are Rust-native extensions: `resolve_pane_identity`, `cleanup_pane_identities`, and `list_agents`.
- All 3 Rust-native tools are covered by dedicated golden fixtures under `tests/conformance/fixtures/rust_native/`.
- The live Rust router exposes 25 logical resource templates after collapsing `?{query}` variants.
- 23 resource templates have Python behavior fixtures.
- 2 Rust-only resources, `resource://tooling/metrics_core` and `resource://tooling/diagnostics`, are unit-tested in `mcp-agent-mail-tools` but still lack conformance fixtures. They are tracked by `br-a2k3h.4` and `br-a2k3h.6`.
- Resource parity coverage remains tracked by `br-a2k3h.4` and `br-a2k3h.6`.
- Python-parity fixture refresh now has a dedicated weekly automation lane via `scripts/regen_python_parity_fixtures.sh` and `.github/workflows/conformance-fixture-regen.yml`.

## Tool Classification

These classifications come from the live tool surface in `mcp_agent_mail_tools::TOOL_CLUSTER_MAP`,
the Python behavior fixture inventory in `tests/conformance/fixtures/python_reference.json`, and
the audit record in [docs/CONFORMANCE_AUDIT_2026-04-18.md](../../docs/CONFORMANCE_AUDIT_2026-04-18.md).

### Python-parity tools (34)

- `health_check` - Return the server readiness snapshot and infrastructure status.
- `ensure_project` - Create or resolve the canonical project identity for a workspace path.
- `install_precommit_guard` - Install the reservation-enforcing Git pre-commit guard.
- `uninstall_precommit_guard` - Remove the reservation-enforcing Git pre-commit guard.
- `register_agent` - Register or refresh an agent identity in a project archive.
- `create_agent_identity` - Mint a fresh agent identity with a unique adjective-noun name.
- `whois` - Inspect an agent profile and optional recent archive commits.
- `send_message` - Create a message, persist recipients, and write archive copies.
- `reply_message` - Reply in-thread while preserving the original thread semantics.
- `fetch_inbox` - Read recent inbox items without mutating archive message contents.
- `mark_message_read` - Mark a delivered message as read for one recipient.
- `acknowledge_message` - Mark a delivered message as read and acknowledged.
- `request_contact` - Request permission to open a contact link with another agent.
- `respond_contact` - Approve or deny a pending contact request.
- `list_contacts` - List the outgoing contact edges for an agent.
- `set_contact_policy` - Set an agent's inbound contact policy.
- `file_reservation_paths` - Reserve project-relative files or globs for coordinated edits.
- `release_file_reservations` - Release one or more active file reservations.
- `renew_file_reservations` - Extend active file reservation expiries in place.
- `force_release_file_reservation` - Break a stale reservation after abandonment heuristics.
- `search_messages` - Query message history through the unified search surface.
- `summarize_thread` - Summarize one thread or an aggregate of multiple threads.
- `macro_start_session` - Ensure project, register agent, reserve files, and fetch inbox in one step.
- `macro_prepare_thread` - Register, summarize a thread, and fetch inbox context in one step.
- `macro_file_reservation_cycle` - Reserve files and optionally auto-release them as a macro flow.
- `macro_contact_handshake` - Request contact, optionally approve it, and optionally send a welcome message.
- `ensure_product` - Create or resolve a product-bus product record.
- `products_link` - Link a project into a product scope.
- `search_messages_product` - Search across all projects linked to a product.
- `fetch_inbox_product` - Read inbox activity for one agent across linked projects.
- `summarize_thread_product` - Summarize a thread across a product's linked projects.
- `acquire_build_slot` - Acquire an advisory build slot lease.
- `renew_build_slot` - Extend an existing build slot lease.
- `release_build_slot` - Release an existing build slot lease.

### Rust-native extensions (3)

- `resolve_pane_identity` - Resolve the canonical agent name for a tmux pane from Rust-side identity files; there is no Python pane-identity analogue.
- `cleanup_pane_identities` - Remove stale per-pane identity files for dead tmux panes; this is Rust-only operational cleanup tied to the pane identity model.
- `list_agents` - List all registered agents in a project; this Rust-native identity surface is now covered by the dedicated `rust_native/` golden fixtures.

Full inventory and the current blocker record live in [docs/CONFORMANCE_AUDIT_2026-04-18.md](../../docs/CONFORMANCE_AUDIT_2026-04-18.md).

Fixture schema and regeneration details remain in [tests/conformance/README.md](tests/conformance/README.md).
