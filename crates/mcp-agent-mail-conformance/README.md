# mcp-agent-mail-conformance

Rust-side conformance harnesses for MCP Agent Mail.

This crate keeps the live Rust router honest against the legacy Python reference where parity is intentional, and it records the remaining Rust-native gaps so drift is explicit instead of accidental.

## Authority boundary

The Rust implementation is the product authority. Python fixtures are a
targeted compatibility and migration safety net, not a mandate to preserve
legacy architecture or defects. A shared contract remains parity-gated only
when an existing client or imported mailbox depends on its observable wire
behavior. Deliberate Rust improvements may diverge when they strengthen
durability, security, bounded work, structured concurrency, or operator
clarity; those changes require native invariant tests and an explicit
compatibility note rather than a weakened implementation.

Accordingly, fixture failures are evidence to classify: accidental wire drift
is a regression, while an intentional and documented Rust-side extension is
not. Release decisions prioritize real SQLite/archive/restart and mounted MCP
proof over byte-for-byte agreement with an obsolete implementation.

The executable legacy-fixture lane applies that boundary directly. Recorded
legacy object fields and values must remain present and equal, and arrays keep
their exact order and cardinality, but additional Rust object fields are
accepted as backwards-compatible extensions. Rust-native golden fixtures
remain exact. Explicitly documented normalization pointers cover intentional
semantic divergence, such as Rust health reporting failing closed when durable
state is unavailable; native invariant tests are authoritative for those fields.

## What lives here

- Behavior parity fixtures in `tests/conformance/fixtures/python_reference.json`
- Regeneration artifacts in `tests/conformance/fixtures/scenario_catalog.json` and `tests/conformance/fixtures/python_reference_regen_report.md`
- Rust-native golden fixtures in `tests/conformance/fixtures/rust_native/`
- Supported tool inventory and input-schema compatibility checks against `../../tests/conformance/fixtures/tool_descriptions.json`; Rust owns current prose
- Resource description drift guards for shared resource templates
- Focused regression checks for tool filtering, error envelopes, and outage behavior
- Automation entrypoints in `../../scripts/regen_python_parity_fixtures.sh` and `.github/workflows/conformance-fixture-regen.yml`

## Current coverage (as of 2026-08-27)

- The live Rust router exposes 44 tools.
- 37 tools have Python behavior fixtures in `tests/conformance/fixtures/python_reference.json`.
- `fetch_topic` is a Rust-native tool covered by its retained description/schema contract plus focused topic-bearing database and router tests; mounted-transport proof remains a release gate. It is intentionally absent from the legacy sequential behavior fixture.
- 7 tools are Rust-native extensions: `fetch_topic`, `resolve_pane_identity`, `cleanup_pane_identities`, `list_agents`, `check_file_reservation_conflicts`, `fetch_inbox_events`, and `get_message_delivery_receipt`.
- The 6 Rust-native tools other than `fetch_topic` are covered by dedicated golden fixtures under `tests/conformance/fixtures/rust_native/`; `fetch_topic` has focused router and persistence coverage for its topic semantics.
- The former tool fixture gap tracked by `br-a2k3h.3` is closed by the dedicated Rust-native fixture lane.
- The live Rust router exposes 25 logical resource templates after collapsing `?{query}` variants.
- 23 resource templates have Python behavior fixtures.
- 2 Rust-only resources, `resource://tooling/metrics_core` and `resource://tooling/diagnostics`, are unit-tested in `mcp-agent-mail-tools` but still lack conformance fixtures. They are tracked by `br-a2k3h.4` and `br-a2k3h.6`.
- Resource parity coverage remains tracked by `br-a2k3h.4` and `br-a2k3h.6`.
- Python-parity fixture refresh now has a dedicated weekly automation lane via `scripts/regen_python_parity_fixtures.sh` and `.github/workflows/conformance-fixture-regen.yml`.

## Tool Classification

These classifications come from the live tool surface in `mcp_agent_mail_tools::TOOL_CLUSTER_MAP`,
the Python behavior fixture inventory in `tests/conformance/fixtures/python_reference.json`, and
the audit record in [docs/CONFORMANCE_AUDIT_2026-04-18.md](../../docs/CONFORMANCE_AUDIT_2026-04-18.md).

### Supported Python compatibility tools (37)

- `health_check` - Return the server readiness snapshot and infrastructure status.
- `ensure_project` - Create or resolve the canonical project identity for a workspace path.
- `install_precommit_guard` - Install the reservation-enforcing Git pre-commit guard.
- `uninstall_precommit_guard` - Remove the reservation-enforcing Git pre-commit guard.
- `register_agent` - Register or refresh an agent identity in a project archive.
- `create_agent_identity` - Mint a fresh agent identity with a unique adjective-noun name.
- `retire_agent` - Remove an identity from active routing while retaining its history.
- `unretire_agent` - Restore a retired identity to active routing.
- `deregister_agent` - Permanently close an identity to new routing without deleting history.
- `whois` - Inspect an agent profile and optional recent archive commits.
- `send_message` - Create a message, persist recipients, and write archive copies.
- `reply_message` - Reply in-thread while preserving the original thread semantics.
- `fetch_inbox` - Read recent inbox items and record read state without mutating archive message contents.
- `mark_message_read` - Mark a delivered message as read for one recipient.
- `acknowledge_message` - Mark a delivered message as read and acknowledged.
- `request_contact` - Request permission to open a contact link with another agent.
- `respond_contact` - Approve or deny a pending contact request.
- `list_contacts` - List contact relationships for an agent.
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

### Rust-native extensions (7)

- `fetch_topic` - Fetch project messages carrying one exact case-insensitive topic tag; this capability has no legacy Python analogue.
- `resolve_pane_identity` - Resolve the canonical agent name for a tmux pane from Rust-side identity files; there is no Python pane-identity analogue.
- `cleanup_pane_identities` - Remove stale per-pane identity files for dead tmux panes; this is Rust-only operational cleanup tied to the pane identity model.
- `list_agents` - List all registered agents in a project; this Rust-native identity surface is now covered by the dedicated `rust_native/` golden fixtures.
- `check_file_reservation_conflicts` - Read-only authoritative conflict check for pre-edit/pre-commit guards (added in `08b05c76`, GH#196); covered by the dedicated `rust_native/` golden fixtures.
- `fetch_inbox_events` - Read durable, body-free recipient delivery events with a restart-safe cursor; covered by the dedicated `rust_native/` golden fixtures.
- `get_message_delivery_receipt` - Read durable per-recipient delivery state for a message; covered by the dedicated `rust_native/` golden fixtures.

Full inventory and the current blocker record live in [docs/CONFORMANCE_AUDIT_2026-04-18.md](../../docs/CONFORMANCE_AUDIT_2026-04-18.md).

Fixture schema and regeneration details remain in [tests/conformance/README.md](tests/conformance/README.md).
