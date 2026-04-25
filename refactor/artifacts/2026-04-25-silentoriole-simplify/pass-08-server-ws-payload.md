# Pass 8 - Server WS Poll Payload Counters

## Files Changed
- `crates/mcp-agent-mail-server/src/tui_ws_state.rs`
- `refactor/artifacts/2026-04-25-silentoriole-simplify/pass-08-server-ws-payload.md`

## Candidate
- Type II clone in `snapshot_payload` and `delta_payload`: both built the same
  `request_counters` JSON object from `state.request_counters()` plus
  `state.avg_latency_ms()`.
- Score: `(LOC_saved 2 * Confidence 5) / Risk 1 = 10.0`.

## Simplification
- Added private `request_counters_json(state: &TuiSharedState) -> Value`.
- Replaced the duplicated inline `request_counters` object in snapshot and
  delta payloads with calls to the helper.

## Isomorphism Card
- Inputs covered: `poll_payload` snapshot mode and delta mode callers in
  `tui_ws_state` tests, plus source inspection of both payload builders.
- Ordering preserved: yes. Snapshot still computes ring, next sequence, events,
  and top-level JSON fields in the same order. Delta still computes ring, events,
  `to_seq`, and top-level JSON fields in the same order.
- JSON schema: unchanged. `request_counters` still contains exactly `total`,
  `status_2xx`, `status_4xx`, `status_5xx`, `latency_total_ms`, and
  `avg_latency_ms`.
- Error semantics: unchanged. The helper is pure JSON construction from existing
  state accessors and introduces no fallible operations.
- Timestamp generation: unchanged. `generated_at_us` still calls `now_micros()`
  from each payload builder.
- Event ordering and limits: unchanged. `recent_events`, `events_since_limited`,
  and `MailEvent::seq` use sites were not touched.
- Observable side effects: unchanged. No transport dispatch, dashboard
  rendering, input handling, TUI state mutation, logging, DB, ATC, or sparkline
  code changed.

## Metrics
- `crates/mcp-agent-mail-server/src/tui_ws_state.rs`: 318 lines before, 314
  lines after, net -4 lines.

## Verification
- `cargo fmt --check`: passed.
- `git diff --check`: passed.
- `cargo test -p mcp-agent-mail-server tui_ws_state --locked`: blocked before
  compilation because Cargo reported the current `Cargo.lock` would need an
  update while `--locked` was set. `Cargo.lock` was not changed because it is
  outside this pass's write scope.

## Proof Limits
- Full workspace tests and clippy were not rerun for this narrow pass.
- Focused server tests could not run under `--locked` until the repository's
  lockfile state is reconciled by the owner of that surface.
