# Pass 4 - Core Env Numeric Parser Simplification

## Change

The integer environment helpers now share a private generic `env_parse` helper.
The public/local typed wrappers remain in place, so call sites and type
inference behavior are unchanged.

## Isomorphism Card

- Inputs covered: `env_u16`, `env_u32`, `env_u64`, and `env_usize` callers.
- Ordering preserved: yes; each wrapper still performs one `env_value` lookup
  and then parses that value.
- Error semantics: unchanged; parse failures still fall back to the provided
  default via `.ok().unwrap_or(default)`.
- Whitespace behavior: unchanged; integer values are still parsed without
  trimming.
- Observable side effects: unchanged; the same `env_value` source precedence is
  used.
- Exclusions: `env_f64` and optional integer parsers were intentionally left
  alone because they have different finite-filtering and empty-string behavior.

## Verification

- `cargo fmt --check` - passed.
- `git diff --check` - passed.
- `cargo test -p mcp-agent-mail-core config::tests:: --locked` - passed, 178 tests.
- `cargo check -p mcp-agent-mail-core --all-targets` - passed.
- `cargo clippy -p mcp-agent-mail-core --all-targets -- -D warnings` - passed.
- `ubs crates/mcp-agent-mail-core/src/config.rs` - ran; reported false-positive
  criticals on environment-variable names such as `HTTP_BEARER_TOKEN` and
  `HTTP_JWT_SECRET`, with fmt/clippy/check/test subchecks clean.
