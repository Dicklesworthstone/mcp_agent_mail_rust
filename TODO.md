# MCP Agent Mail Rust Port — TODO (Granular)

## 0. Coordination / Meta
- [x] Read `AGENTS.md` and `README.md` (baseline orientation).
- [x] Register with MCP Agent Mail and announce to active agents.
- [x] Check inbox from other agents at least once per session.
- [x] Reserve files before edits (Agent Mail leases) and release on completion.
- [x] Keep `FEATURE_PARITY.md` aligned with completed work.
- [x] Keep `TODO.md` updated after each batch of changes.
- [ ] Confirm MCP Agent Mail server stability (health check path, lock handling).

## 1. Workspace & Crate Scaffolding
- [x] Workspace `Cargo.toml` with required path deps.
- [x] `rust-toolchain.toml` with nightly channel.
- [x] Crate directories for core/db/storage/server/cli/guard/share/conformance.
- [x] Scaffold minimal `Cargo.toml` for each crate.
- [x] Placeholder `lib.rs`/`main.rs` for each crate.
- [x] README for Rust port (overview, usage, dev commands).

## 2. Conformance Harness (Skeleton → Full)
- [x] `crates/mcp-agent-mail-conformance/tests/conformance/README.md` with schema.
- [x] Fixtures placeholder JSON + loader.
- [x] Python fixture generator stub + full runner.
- [x] Rust test to load fixture schema.
- [x] Python reference runner: start legacy server + capture tool outputs.
- [x] Python reference runner: capture resources + deterministic ordering.
- [x] Canonical fixture schema for ok/err shapes.
- [x] Rust conformance runner: tools + resources + diff reporting.
- [x] Tool-filter fixtures (profiles + include/exclude).
- [x] Archive artifact assertions (profiles, messages, inbox/outbox, reservations).
- [x] Notification signal conformance (to/cc signals, bcc excluded, fetch_inbox clears).
- [x] Conformance CLI helper to regenerate fixtures (Rust wrapper or script).
- [x] CI step for conformance tests (ensure fixtures are current).
- [x] Add missing error-case fixtures (invalid params, unknown project/agent, malformed inputs).
- [x] Add resource query-param fixtures (format/filter params for static resources).

## 3. DB Layer (sqlmodel_rust) — Models & Schema
- [x] SQLModel structs for all tables (`src/models.rs`).
- [x] JSON attachments field in message schema.
- [x] Timestamp conversion utilities (`timestamps.rs`).
- [x] Error types (`error.rs`).
- [x] Explicit indexes + unique constraints matching legacy.
- [x] FTS triggers installed and validated.
- [x] Schema migration story using `sqlmodel_schema::MigrationRunner`.
- [x] WAL PRAGMA setup SQL (`schema.rs`).
- [x] Pool config defaults (`pool.rs`).
- [x] Pool wired to `sqlmodel_pool` + `sqlmodel_sqlite`.
- [x] Query tracking + slow query logging (per-tool instrumentation).
- [x] Verify tracked connection coverage for any direct `query_sync` tool paths.

## 4. Server Layer (fastmcp_rust)
- [x] All tools registered with correct names and clusters.
- [x] All resources registered with correct URI templates.
- [x] Tool filtering profiles wired (full/core/minimal/messaging/custom).
- [x] Identity tools: ensure_project/register_agent/create_agent_identity/whois.
- [x] Messaging tools: send_message/reply_message/fetch_inbox/mark_message_read/acknowledge_message.
- [x] Search tools: search_messages/summarize_thread.
- [x] File reservation tools: file_reservation_paths/release/renew/force_release.
- [x] Contact tools: request_contact/respond_contact/list_contacts/set_contact_policy.
- [x] Product tools: ensure_product/products_link/search_messages_product/fetch_inbox_product/summarize_thread_product.
- [x] Build slots: acquire/renew/release.
- [x] Macros: start_session/prepare_thread/file_reservation_cycle/contact_handshake.
- [x] TOON output handling for tool + resource responses.
- [x] Instrumentation wrapper for per-tool query stats logging.
- [ ] Validate resource query matching behavior against fastmcp router (edge cases).
- [x] Implement query parsing for static resources (`resource://projects?format=...`).

## 5. Storage Layer (Git Archive)
- [x] Archive root creation + `.gitattributes`.
- [x] Per-project locks (`.archive.lock`) + commit locks.
- [x] Commit queue batching.
- [x] Message write pipeline (canonical + inbox/outbox + commit).
- [x] File reservation artifacts (digest + id files).
- [x] Attachment pipeline (WebP conversion, originals, manifests, audit logs).
- [x] Notification signals (debounced files).
- [x] Read helpers (message by id, thread digest, commit history).
- [x] Storage wired into identity, messaging, reservation, resources.

## 6. Guard (Pre-commit/Pre-push)
- [x] Guard install/uninstall (chain-runner hook).
- [x] Pre-commit check (staged paths, rename handling).
- [x] Pre-push check (commit diff scanning).
- [x] Path matching (pathspec + fnmatch fallback).
- [x] Advisory mode + bypass env var.
- [x] Rust-native conflict detection (guard_check, guard_check_full).
- [x] Guard status command.

## 7. Share / Export
- [x] SQLite snapshot (WAL checkpoint + backup).
- [x] Scrub presets (standard/strict/archive).
- [x] FTS + materialized views for static viewer.
- [x] Attachment bundling (inline/detach thresholds).
- [x] Chunking for large DB.
- [x] Manifest + signing + encryption (age/Ed25519).
- [x] Preview server + verify/decrypt.

## 8. CLI
- [x] `serve-http`, `serve-stdio`.
- [x] `lint`, `typecheck`.
- [x] `guard` subcommands (install/uninstall/status/check).
- [x] `acks` subcommands (pending/remind/overdue).
- [x] `file_reservations` subcommands (list/active/soon).
- [x] `config` subcommands (set-port/show-port).
- [x] `doctor check` (--verbose, --json).
- [x] `projects` subcommands (mark-identity/discovery-init/adopt).
- [x] `list-projects` (--json, --include-agents).
- [x] `list-acks`.
- [x] `mail status`.
- [x] `migrate`.
- [x] `clear-and-reset-everything`.
- [x] `am-run` and `amctl env`.
- [x] `share` subcommands (export/update/preview/verify/decrypt/wizard).
- [x] `archive` subcommands (save/list/restore).
- [x] `doctor` subcommands (repair/backups/restore).
- [x] `products` subcommands (ensure/link/status/search/inbox/summarize-thread).
- [x] `docs insert-blurbs`.

## 9. Benchmarks (Perf + Conformance)
- [x] Bench suite reusing conformance fixtures.
- [x] Tool latency baseline (8 representative tools).
- [x] Initial baseline recorded (most tools sub-100µs).
- [x] Add archive write throughput benchmarks (send_message + commit queue).
- [x] Capture hyperfine baselines for CLI commands (serve + send_message + fetch_inbox).
- [x] Run `cargo flamegraph` for hot paths (search + send_message + storage writes).
- [x] Record golden outputs + checksums for optimization validation.
- [ ] Maintain perf regression notes in `FEATURE_PARITY.md` or dedicated doc.

## 10. Dependency Integration (Local /dp Crates)
- [x] Confirm zero direct `tokio` usage in code paths (only via optional deps).
- [x] Use `sqlmodel_rust` for SQLite access (no direct `rusqlite`).
- [x] Use `asupersync` for HTTP + async runtime.
- [x] Use `frankentui` (`ftui`) for CLI rendering.
- [ ] Integrate `beads_rust` for issue/task awareness in CLI or server.
- [ ] Integrate `coding_agent_session_search` for agent detection (or port logic to avoid tokio).
- [ ] Evaluate fastmcp resource query param handling; add adapters if needed.

## 11. Testing & QA
- [x] Unit tests for tool validation rules (input constraints, error messages).
- [x] Integration tests for DB + storage pipelines (archive + SQLite consistency).
- [x] E2E tests for MCP tool calls (stdio + HTTP).
- [x] Guard tests for conflict detection (pathspec + rename + advisory).
- [x] Run full toolchain: `cargo check --all-targets`, `cargo clippy --all-targets -- -D warnings`, `cargo fmt --check`, `cargo test`.

## 12. Publishing & Release (Crates.io + CI)
- [ ] Confirm Cargo metadata (license/readme/repository) for publishable crates.
- [ ] Add crates.io publish workflow gated by tags (`CARGO_REGISTRY_TOKEN`).
- [ ] Document publish order for workspace crates (`cargo publish -p ...`).
