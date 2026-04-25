# Isomorphism Card

Run: `2026-04-24-proudridge-simplify`
Base commit: `1c2fbb3c`

## Change: D1 local helper for messaging agent-name normalization

### Equivalence contract
- **Inputs covered:** `send_message.sender_name`, `send_message.to/cc/bcc`, `reply_message.sender_name`, `reply_message.to/cc/bcc`, `fetch_inbox.agent_name`, `mark_message_read.agent_name`, `acknowledge_message.agent_name`.
- **Ordering preserved:** yes. Recipient vectors still use `into_iter().map(...).collect()` in original order.
- **Tie-breaking:** unchanged / N/A.
- **Error semantics:** unchanged. Invalid names still fall back to the original owned `String` via `unwrap_or(name)`.
- **Laziness:** unchanged. All values were eagerly normalized before validation and DB lookups; they remain eagerly normalized.
- **Short-circuit eval:** unchanged / N/A.
- **Floating-point:** N/A.
- **RNG / hash order:** N/A.
- **Observable side-effects:** unchanged. `normalize_agent_name` is pure; logging, DB writes, archive writes, and notification order are untouched.
- **Type narrowing:** Rust ownership remains equivalent: helper consumes `String` and returns `String`; optional vectors stay `Option<Vec<String>>`.
- **Rerender behavior:** N/A.

### Verification plan
- `cargo fmt --check` passed.
- `rch exec -- cargo check --workspace --all-targets` passed.
- Focused tests passed: `rch exec -- cargo test -p mcp-agent-mail-tools --test messaging_error_parity` (8 tests).
- `rch exec -- cargo clippy --workspace --all-targets -- -D warnings` is currently blocked by dirty sibling work in `/data/projects/frankensqlite`, outside this refactor.
- Full workspace tests were not run after the clippy blocker because the current dependency graph does not compile through the dirty sibling path dependency.

## Baseline note: R1 rustfmt drift

`cargo fmt --check` failed before D1. The formatting-only normalization touches `crates/mcp-agent-mail-cli/src/lib.rs` and `crates/mcp-agent-mail-db/src/reconstruct.rs` and has no semantic change.
