# Pass 3 - Search Helper Simplification

## Change

`resolve_time_bound_alias` no longer allocates a temporary candidate vector.
It streams the canonical value first and then the alias slice through an
iterator chain.

## Isomorphism Card

- Inputs covered: canonical date bound plus all start/end alias values.
- Ordering preserved: yes; canonical value is still evaluated first, aliases
  are still evaluated in slice order.
- Tie-breaking: unchanged; the first non-empty equivalent value still wins, and
  conflicting later values still produce the same alias conflict error.
- Error semantics: unchanged; loop body and `alias_conflict_error` calls are
  unchanged.
- Laziness: slightly improved; candidates are no longer materialized before the
  loop, but each alias `Option<String>` is still cloned before trimming exactly
  as before.
- Short-circuit evaluation: unchanged for conflict returns; the same early
  return remains in the loop body.
- Observable side effects: unchanged; no logging, DB, search planner, or JSON
  response behavior changed.

## Verification

- `cargo fmt --check` - passed.
- `git diff --check` - passed.
- `cargo test -p mcp-agent-mail-tools search::tests:: --locked` - passed, 58 tests.
- `cargo check -p mcp-agent-mail-tools --all-targets` - passed.
- `cargo clippy -p mcp-agent-mail-tools --all-targets -- -D warnings` - passed.
- `ubs crates/mcp-agent-mail-tools/src/search.rs` - ran; reported a false
  positive critical on local variable `token` in `parse_importance_list`, with
  fmt/clippy/check/test subchecks clean.
