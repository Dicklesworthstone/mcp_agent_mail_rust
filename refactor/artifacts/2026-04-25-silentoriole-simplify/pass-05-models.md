# Pass 5 - Core Model Detection Helper Simplification

## Change

The four agent-name mistake classifiers that share the same trim,
empty-check, and ASCII-lowercase prelude now call `lowercase_non_empty`.

## Isomorphism Card

- Inputs covered: program-name, model-name, broadcast-token, and descriptive
  role-name detection helpers.
- Ordering preserved: yes; `detect_agent_name_mistake` call order is unchanged.
- Error semantics: unchanged; the functions still return booleans only, and
  the caller's exact message text was not touched.
- Case/whitespace behavior: unchanged; each helper still trims first, rejects
  empty input, and lowercases with `to_ascii_lowercase`.
- Observable side effects: none.
- Exclusions: email detection, Unix username detection, vocabulary arrays, and
  generated-name logic were intentionally untouched.

## Verification

- `cargo fmt --check` - passed.
- `git diff --check` - passed.
- `cargo test -p mcp-agent-mail-core models::tests:: --locked` - passed, 71 tests.
- `cargo check -p mcp-agent-mail-core --all-targets` - passed.
- `cargo clippy -p mcp-agent-mail-core --all-targets -- -D warnings` - passed.
- `ubs crates/mcp-agent-mail-core/src/models.rs` - no critical findings.
