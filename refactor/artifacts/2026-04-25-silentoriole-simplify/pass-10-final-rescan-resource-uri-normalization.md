# Pass 10 Final Rescan: Resource URI Normalization

## Change

Collapse the repeated `normalize_fixture_resource_uri` prefix checks into one ordered local mapping and loop.

## Opportunity Matrix

| Candidate | LOC | Confidence | Risk | Score | Decision |
| --- | --- | --- | --- | --- | --- |
| `normalize_fixture_resource_uri` fixed prefix mapping | 1 | 5 | 1 | 5.0 | Apply |

## Isomorphism Card

- Inputs covered: every fixture resource URI normalized by `normalize_fixture_resource_uri`.
- Ordering preserved: yes; the mapping preserves the original branch order exactly, including `resource://mailbox-with-commits/` before `resource://mailbox/`.
- Tie-breaking: unchanged; first matching prefix still wins.
- Error semantics: unchanged; no new error path or panic path.
- Laziness: unchanged for callers; prefix checks still short-circuit on the first match.
- Floating-point: N/A.
- RNG / hash order: N/A.
- Observable side effects: unchanged; the function is pure and emits no logs, metrics, DB writes, or archive writes.
- Type narrowing: N/A.
- Fallback: unchanged; `uri.split('?').next().unwrap_or(uri).to_string()` remains the final fallback.

## Verification

- Pre-edit baseline: `cargo test -p mcp-agent-mail-conformance --test resource_coverage_guard --locked` passed, 1 test.
- Post-edit: `cargo fmt --check` passed.
- Post-edit: `git diff --check` passed.
- Worker post-edit: `cargo test -p mcp-agent-mail-conformance --test resource_coverage_guard --locked` passed, 1 test, before the transient lockfile delta was removed.
- Local clean-tree note: `cargo test -p mcp-agent-mail-conformance --test resource_coverage_guard --locked` is blocked by the repository's stale `Cargo.lock` state.
- Local post-edit: `cargo test -p mcp-agent-mail-conformance --test resource_coverage_guard` passed, 1 test.
- Local post-edit: `cargo check -p mcp-agent-mail-conformance --all-targets` passed.
- Local post-edit: `cargo clippy -p mcp-agent-mail-conformance --all-targets -- -D warnings` passed.
- Local post-edit: `ubs crates/mcp-agent-mail-conformance/tests/resource_coverage_guard.rs` reported one existing test-only `panic!` inventory finding outside the changed prefix-table lines; UBS fmt/clippy/check/test-build subchecks were clean.

## LOC Ledger

| Path | Before | After | Delta |
| --- | ---: | ---: | ---: |
| `crates/mcp-agent-mail-conformance/tests/resource_coverage_guard.rs` | 237 | 234 | -3 |

## Preservation Proof

- Prefix order is unchanged from the original branch chain.
- Exact normalized URI strings are unchanged.
- `resource://mailbox-with-commits/` remains before `resource://mailbox/`.
- First-match short-circuit behavior is unchanged.
- The final query-stripping fallback is unchanged.
