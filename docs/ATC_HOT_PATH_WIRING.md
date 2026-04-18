# ATC Hot-Path Wiring

This note records the v1 hot-path ATC seams that run inline with MCP tool dispatch.
The goal is to capture durable, audit-friendly learning signals without changing tool outcomes.

## Message Observation

`send_message` and `reply_message` append a single `message_sent` ATC experience row keyed by `context.message_id`.
`fetch_inbox` and `fetch_inbox_product` append independent `message_received` rows for each delivered inbox item.
These are side-channel observations only: tool success is not coupled to ATC persistence.

## Build Slot Observation

`acquire_build_slot`, `renew_build_slot`, and `release_build_slot` append
`build_slot_acquired`, `build_slot_renewed`, and `build_slot_released`
ATC experience rows when `WORKTREES_ENABLED=true`.

Each row carries the normalized project key, agent, slot name, and compact
slot/branch hashes plus TTL/conflict metadata in context/feature-extension
fields so the seam remains durable without widening the fixed hot-path feature
vector.

When `WORKTREES_ENABLED=false`, the hot-path recorder returns immediately and
does not create any ATC experience rows. The feature gate is enforced both by
the tool surface and by the ATC seam itself.

## Ack And Read Resolution

`acknowledge_message(message_id)` resolves the matching `message_sent` row to outcome label `acknowledged`.
`mark_message_read(message_id)` resolves the same send-side row to outcome label `read`.
`message_received` rows are intentionally not resolved by ack/read; they remain separate inbox-behavior observations.

## Ack Wins Over Read

If a send-side row has already resolved as `read`, a later `acknowledge_message` upgrades that resolved outcome payload to `acknowledged`.
If a row is already `acknowledged`, later read events are no-ops.
This preserves a monotone lifecycle state while still recording the stronger terminal label.

## Missing Or Racy Rows

Hot-path resolution retries the `message_sent` lookup once before giving up.
If the message exists but no ATC row is present, the event is treated as pre-learning-era and logged at debug.
If the `message_id` is unknown entirely, the tool still succeeds and ATC logs a warning.

## Write Modes

In `live` mode, hot-path seams write to the durable ATC store when the store is writable.
In `shadow` mode, seams emit tracing and shadow metrics only; they do not mutate ATC rows.
In `off` mode, the hot-path ATC hooks return immediately.
