# Refactor Ledger

| ID | Status | Files | Before | After | Delta | Verification |
|----|--------|-------|--------|-------|-------|--------------|
| R1 | done | `crates/mcp-agent-mail-cli/src/lib.rs`, `crates/mcp-agent-mail-db/src/reconstruct.rs` | `cargo fmt --check` failed | `cargo fmt --check` passed | `10 insertions, 22 deletions` | `cargo fmt --check` |
| D1 | done | `crates/mcp-agent-mail-tools/src/messaging.rs` | 5898 lines | 5885 lines | `26 insertions, 39 deletions` | `cargo fmt --check`; `rch exec -- cargo test -p mcp-agent-mail-tools --test messaging_error_parity`; `rch exec -- cargo check --workspace --all-targets` |
| F1 | done | `crates/mcp-agent-mail-db/src/reconstruct.rs` | `summary.contains(&format!("{}", n))` | named `expected_collision_count` string | `1 insertion, 0 deletions` | `cargo fmt --check`; `git diff --check` |
| A1 | done | `duplication_map.md` | scaffold/empty candidate state | implemented candidate inventory | artifact-only | Markdown reviewed after update |

## Gate Notes

- `cargo fmt --check`: passed after R1 and D1.
- `git diff --check`: passed after F1.
- `duplication_map.md`: reviewed after A1 and retained as the durable candidate inventory.
- `rch exec -- cargo test -p mcp-agent-mail-tools --test messaging_error_parity`: passed, 8 tests.
- `rch exec -- cargo check --workspace --all-targets`: passed before the sibling path dependency changed.
- `rch exec -- cargo clippy --workspace --all-targets -- -D warnings`: blocked by dirty sibling work in `/data/projects/frankensqlite` (`crates/fsqlite-btree/src/cursor.rs` calls `try_table_append_on_hinted_leaf_with_known_last_rowid` with 5 arguments while the local dirty signature takes 6).
- `rch exec -- cargo test -p mcp-agent-mail-db finalize_cross_project_canonical_collision_warnings_emits_summary_above_sample_limit`: stopped after it waited on the remote artifact lock for about two minutes without reaching compilation or test execution.
