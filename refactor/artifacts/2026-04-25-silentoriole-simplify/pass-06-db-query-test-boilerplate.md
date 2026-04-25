# Pass 6 - DB Query/Test Boilerplate

## Files Changed
- `crates/mcp-agent-mail-db/src/reconstruct.rs`

## Simplification
- Added a test-only `message_one_recipients_json` helper that preserves the exact
  `SELECT recipients_json FROM messages WHERE id = 1` query, row `0` lookup,
  `recipients_json` column extraction, and JSON parsing used by two recipient
  reconstruction tests.
- Replaced the two duplicated query/extract/parse blocks with calls to the
  helper.

## Fresh-Eyes Fix
- The focused test lane exposed an adjacent existing bug: an existing agent row
  whose name trimmed to empty was emitted as `[unknown-agent-N]`.
- Adjusted `sync_reconstructed_message_recipients_json` so only missing joined
  agent rows get the unknown-agent sentinel. Existing blank names now flow
  through as blank and are dropped by `normalized_archive_agent_name`, while
  orphaned recipient rows remain visible.

## Preservation Proof
- No schema, archive, salvage map, canonical ID, or warning text behavior was
  changed.
- Missing-agent sentinel behavior is still pinned by
  `sync_reconstructed_message_recipients_json_keeps_orphaned_recipient_rows_visible`.
- Blank existing-agent drop behavior is now pinned by
  `sync_reconstructed_message_recipients_json_trims_and_drops_blank_names`.

## Verification
- `cargo fmt --check`
- `git diff --check`
- `cargo test -p mcp-agent-mail-db sync_reconstructed_message_recipients_json --locked`
- `cargo check -p mcp-agent-mail-db --all-targets`
- `cargo clippy -p mcp-agent-mail-db --all-targets -- -D warnings`
- `ubs crates/mcp-agent-mail-db/src/reconstruct.rs` (0 critical; warning inventory only)
