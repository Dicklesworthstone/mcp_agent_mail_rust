# Pass 09 - Resource Description Test Harness

## Candidate

- File: `crates/mcp-agent-mail-conformance/tests/resource_description_parity.rs`
- Function: `expected_description_prefixes()`
- Pattern: Type II test-harness boilerplate clone
- Change: replace repeated `m.insert(key, value)` calls with one literal tuple table collected into the same `HashMap<&'static str, &'static str>` return type.

## Score

- LOC saved score: 3 (actual test-file line count 281 -> 241, net -40)
- Confidence: 5 (single pure literal map builder; same 21 key/value pairs)
- Risk: 1 (single test file, no production/runtime path)
- Score: 15

## Isomorphism Card

- Inputs covered: the same static 21 resource URI and expected-description-prefix pairs consumed by `resource_descriptions_match_python_prefixes`.
- Ordering preserved: assertions do not depend on `HashMap` iteration order; the tuple table lists entries in the same source order.
- Tie-breaking: N/A.
- Error semantics: unchanged; no new fallible operations or panics.
- Laziness: unchanged for callers; the function still returns a fully materialized `HashMap`.
- Short-circuit eval: unchanged; assertion loop logic is untouched.
- Floating-point: N/A.
- RNG / hash order: unchanged contract; callers already consumed a `HashMap` with unspecified iteration order.
- Observable side effects: unchanged; no logging, env mutation, resource collection, or assertions touched.
- Type narrowing: unchanged Rust return type: `HashMap<&'static str, &'static str>`.
- Rerender behavior: N/A.

## Baseline

- `cargo fmt --check`: pass before edit.
- `git diff --check`: pass before edit.
- `cargo test -p mcp-agent-mail-conformance --test resource_description_parity --locked`: blocked before compilation because Cargo wanted to update `Cargo.lock` while `--locked` was set. No lockfile edit was made as part of that attempt.
- `wc -l crates/mcp-agent-mail-conformance/tests/resource_description_parity.rs`: 281 before edit.

## Fresh-Eyes Verification

- Manual review found the code behavior preserved; the only Rust code change is `expected_description_prefixes()` data construction.
- Exact string-literal preservation check for the function returned no diff:
  `diff -u <(git show HEAD:crates/mcp-agent-mail-conformance/tests/resource_description_parity.rs | sed -n '/fn expected_description_prefixes/,/^}/p' | rg -o '"[^"]+"') <(sed -n '/fn expected_description_prefixes/,/^}/p' crates/mcp-agent-mail-conformance/tests/resource_description_parity.rs | rg -o '"[^"]+"')`
- `cargo fmt --check`: pass after edit.
- `git diff --check`: pass after edit.
- `wc -l crates/mcp-agent-mail-conformance/tests/resource_description_parity.rs`: 241 after edit.
- `cargo test -p mcp-agent-mail-conformance --test resource_description_parity --locked`: pass after edit, 3 passed, 0 failed.
- `cargo check -p mcp-agent-mail-conformance --all-targets --locked`: pass.
- `cargo clippy -p mcp-agent-mail-conformance --all-targets --locked -- -D warnings`: pass.
- `ubs crates/mcp-agent-mail-conformance/tests/resource_description_parity.rs`: reported one existing test-only `panic!` inventory finding plus warning inventory; fmt/clippy/check/test-build subchecks were clean.
- Any transient `Cargo.lock` resolver side effects from earlier interrupted/unlocked cargo work were manually removed; the unrelated `crates/mcp-agent-mail-share/src/finalize.rs` dirty change was left untouched.
