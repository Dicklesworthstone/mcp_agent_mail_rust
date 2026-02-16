# Gemini Fixes Summary (Part 3) - 2026-02-16

## Critical Bug Fixes

### 1. `reply_message` Logic Parity with `send_message`
- **Component:** `crates/mcp-agent-mail-tools`
- **Issue:** The `reply_message` implementation lacked two critical features present in `send_message`:
    1.  **Contact Policy Enforcement:** Replies bypassed the contact policy checks, allowing agents to message blocked or non-consented recipients simply by replying.
    2.  **Recipient Auto-Registration:** Replies to new recipients (e.g. adding a CC) failed with `RECIPIENT_NOT_FOUND` instead of auto-registering them if configured.
- **Fix:** Refactored `reply_message` in `crates/mcp-agent-mail-tools/src/messaging.rs` to use the shared `push_recipient` logic (enabling auto-registration) and copied the rigorous contact enforcement block from `send_message` (checking thread history, file reservations, and contact links).

## Verification
- Verified `reply_message` now performs the same resolution and enforcement steps as `send_message`.
- Confirmed `auto_contact_if_blocked` behavior logic is included (using config default).
