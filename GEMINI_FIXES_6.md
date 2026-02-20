# Gemini Fixes Summary (Part 6) - 2026-02-20

## Bug Fixes

### 1. `budget_deadline_is_relative_to_wall_now_not_absolute` Test Logic
- **Component:** `crates/mcp-agent-mail-server`
- **Issue:** The regression test `budget_deadline_is_relative_to_wall_now_not_absolute` contained a logical error. It attempted to check `.as_nanos()` on an opaque `Instant` type (returned by `wall_now()`), which is not valid usage of the `asupersync` time API. The test intention was correct (verify relative vs absolute deadline), but the implementation was flawed.
- **Fix:** Revised the test to use `deadline.saturating_duration_since(check_time).as_secs()` to correctly measure the remaining time until the deadline, ensuring the test robustly verifies the fix for the original bug (where absolute epoch time was used for relative deadlines).

### 2. CLI `guard install` Pre-push Flag Support
- **Component:** `crates/mcp-agent-mail-cli`
- **Issue:** The `handle_guard` function in the CLI ignored the `prepush` and `no_prepush` command-line flags, failing to pass the user's intent to `install_guard`. This effectively meant the pre-push hook status was hardcoded (or undefined) regardless of CLI arguments.
- **Fix:** Updated `handle_guard` in `crates/mcp-agent-mail-cli/src/lib.rs` to:
    1.  Calculate `install_prepush` based on the flags (`if prepush { true } else { !no_prepush }`), defaulting to `true`.
    2.  Pass this boolean to the updated `mcp_agent_mail_guard::install_guard` function.

## Verification
- **Test Logic:** Verified the new test logic uses standard `Duration` methods available on `Instant`.
- **CLI Logic:** Confirmed the flag precedence logic matches the intended behavior (explicit enable > explicit disable > default enable).
- **Compilation:** Verified call sites match the new function signatures.
