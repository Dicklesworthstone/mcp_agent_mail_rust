# Gemini Fixes Round 7 - 2026-02-20

## Summary
Hardened TUI input handling against DoS, disabled unsupported broadcast messaging, and prevented DB bloat from large payloads.

## Changes

### 1. TUI Input Hardening (`crates/mcp-agent-mail-server`)
- **File:** `src/tui_ws_input.rs`
- **Issue:** The `IngressInputEvent::Key` struct accepted unbounded strings. A malicious client could flood the event queue with large payloads (up to 512KB per request), potentially causing OOM or log spam.
- **Fix:** Truncated the `key` field to 4096 bytes (respecting UTF-8 boundaries) during parsing, before it enters the `RemoteTerminalEvent` queue.

### 2. Messaging Safety (`crates/mcp-agent-mail-tools`)
- **File:** `src/messaging.rs`
- **Issue:** The `broadcast` feature in `send_message` was enabled but is explicitly unsupported by policy.
- **Fix:** Disabled the broadcast logic. Attempting to use `broadcast=true` now returns an `INVALID_ARGUMENT` error stating "Broadcast messaging is not supported."

### 3. Reservation Safety (`crates/mcp-agent-mail-tools`)
- **File:** `src/reservations.rs`
- **Issue:** `force_release_file_reservation` accepted an unbounded `note` parameter which was inserted directly into the database as a message body, bypassing standard message size limits.
- **Fix:** Truncated the `note` to 4096 bytes before using it in the notification message.

## Verification
- Verified via static analysis that `truncate_utf8` logic (reimplemented locally or inline) preserves valid UTF-8.
- Verified `send_message` logic flow ensures `broadcast=true` fails fast.
- Verified `force_release_file_reservation` uses the truncated note for both DB insert and archive write.
