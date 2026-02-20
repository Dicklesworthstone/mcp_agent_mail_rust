# Gemini Fixes Summary (Part 4) - 2026-02-20

## Critical Bug Fixes

### 1. `send_message` Broadcast Contact Policy Enforcement
- **Component:** `crates/mcp-agent-mail-tools`
- **Issue:** When `broadcast=true` is used in `send_message`, the recipient list is auto-populated with all active agents. However, the subsequent contact policy enforcement loop would error out (fail the entire call) if *any* of those agents had a restrictive policy (e.g. `contacts_only` without a link). This made broadcast unusable in projects with mixed contact policies.
- **Fix:** Updated the enforcement loop in `send_message` to detect broadcast mode. When broadcasting, restrictive policies (`BlockAll`, `RequireApproval`) now cause the recipient to be silently dropped from the distribution list instead of raising an error. This ensures the message reaches all willing recipients.

### 2. `release_file_reservations` Race Condition & Artifact Update
- **Component:** `crates/mcp-agent-mail-db`, `crates/mcp-agent-mail-tools`
- **Issue:** `release_file_reservations` previously relied on a "fetch candidates -> release" pattern that was prone to race conditions (fetching a reservation that gets released by another process before the release call). Additionally, the return type `usize` (count) didn't provide enough info to accurately write updated JSON artifacts to the git archive, leading to divergence between DB (released) and Git (active).
- **Fix:**
    - Modified `release_reservations` in `crates/mcp-agent-mail-db/src/queries.rs` to use `UPDATE ... RETURNING ...`, returning the exact `Vec<FileReservationRow>` modified by the transaction.
    - Updated `release_file_reservations` in `crates/mcp-agent-mail-tools/src/reservations.rs` to use these returned rows. It now correctly identifies exactly which reservations were released and writes updated JSON artifacts (with `released_ts`) to the git archive via the Write-Behind Queue (WBQ).

## Verification
- Verified logic flow for `send_message` ensures `all_recipients` is correctly filtered when recipients are dropped.
- Verified `release_reservations` SQL syntax uses standard SQLite `RETURNING` clause.
- Confirmed `release_file_reservations` handles the new `Vec` return type and preserves the API response format (count).
