# Pass 2 - Build Slot Lease Helper Simplification

## Change

`collect_slot_conflicts` now pushes a conflict when
`request_exclusive || entry.exclusive` instead of first materializing the same
boolean through a temporary branch.

## Isomorphism Card

- Inputs covered: all active leases passed to `collect_slot_conflicts`.
- Ordering preserved: yes; the loop order and earlier `continue` filters are
  unchanged.
- Tie-breaking: unchanged; conflict order still follows active lease order.
- Error semantics: N/A; the helper does not return errors.
- Short-circuit evaluation: equivalent; when `request_exclusive` is true, the
  old branch returned true without reading `entry.exclusive`, and the new `||`
  expression does the same.
- Observable side effects: unchanged; only the same `conflicts.push(entry)` call
  remains.
- On-disk compatibility: unchanged; lease-path helpers and JSON serialization
  were not touched.

## Verification

- `cargo fmt --check` - passed.
- `git diff --check` - passed.
- `cargo test -p mcp-agent-mail-tools build_slots --locked` - passed, 40 tests.
- `cargo check -p mcp-agent-mail-tools --all-targets` - passed.
- `cargo clippy -p mcp-agent-mail-tools --all-targets -- -D warnings` - passed.
- `ubs crates/mcp-agent-mail-tools/src/build_slots.rs` - no critical findings;
  warnings were existing test unwrap/assert and inventory-style findings.
