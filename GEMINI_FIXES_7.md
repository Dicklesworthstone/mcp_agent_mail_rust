# Gemini Fixes Round 7 - 2026-02-20

## Summary
Hardened TUI input handling against DoS, improved messaging UX, and prevented DB bloat from large payloads.

## Changes

### 1. TUI Input Hardening (`crates/mcp-agent-mail-server`)
- **File:** `src/tui_ws_input.rs`
- **Issue:** The `IngressInputEvent::Key` struct accepted unbounded strings. A malicious client could flood the event queue with large payloads (up to 512KB per request), potentially causing OOM or log spam.
- **Fix:** Truncated the `key` field to 4096 bytes (respecting UTF-8 boundaries) during parsing, before it enters the `RemoteTerminalEvent` queue.

### 2. Messaging UX (`crates/mcp-agent-mail-tools`)
- **File:** `src/messaging.rs`
- **Issue:** When `send_message` was called with `broadcast=true` but no recipients were eligible (due to contact policies or inactivity), the error message was the generic "At least one recipient is required", confusing users who thought they *did* provide recipients via the broadcast flag.
- **Fix:** Added a specific error message for empty broadcast results, directing the user to check active agents.

### 3. Reservation Safety (`crates/mcp-agent-mail-tools`)
- **File:** `src/reservations.rs`
- **Issue:** `force_release_file_reservation` accepted an unbounded `note` parameter which was inserted into the database as a message body, bypassing the standard `max_message_body_bytes` checks enforced by `send_message`.
- **Fix:** Truncated the `note` to 4096 bytes before using it in the notification message.

## Verification
- Verified via static analysis that `truncate_utf8` logic (reimplemented locally or inline) preserves valid UTF-8.
- Verified `send_message` logic flow ensures the new error path is reachable only when `broadcast` was requested.
- Verified `force_release_file_reservation` uses the truncated note for both DB insert and archive write.
