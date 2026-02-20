# Gemini Fixes Summary (Part 5) - 2026-02-20

## Critical Bug Fixes

### 1. Pre-push Hook Installation Support
- **Component:** `crates/mcp-agent-mail-guard` and `crates/mcp-agent-mail-tools`
- **Issue:** The `install_guard` function in `mcp-agent-mail-guard` ignored the pre-push hook entirely, despite the logic for checking pushed files (`get_push_paths`) existing in the crate. This meant the pre-push protection (checking commits before they are pushed) was never installed, leaving a gap in enforcement. Additionally, the CLI tool `install_precommit_guard` didn't expose an option to install it, or default to it.
- **Fix:**
    - Updated `install_guard` signature in `crates/mcp-agent-mail-guard/src/lib.rs` to accept `install_prepush: bool`.
    - Implemented installation logic for `pre-push` hook (chain-runner, Windows shims, and plugin script) within `install_guard`.
    - Updated `install_precommit_guard` in `crates/mcp-agent-mail-tools/src/reservations.rs` to pass `true` for `install_prepush`, restoring legacy behavior where both hooks were installed by default.

## Verification
- Verified `install_guard` now iterates over both "pre-commit" and "pre-push" when requested.
- Verified `render_chain_runner_script` handles the `pre-push` logic (reading stdin) correctly when `hook_name` is "pre-push".
- Confirmed `install_precommit_guard` in the tools crate compiles with the updated signature.
