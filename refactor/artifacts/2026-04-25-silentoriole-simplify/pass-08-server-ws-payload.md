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
- Added private
  `request_counters_json(counters: RequestCounters, avg_latency_ms: u64) -> Value`.
- Replaced the duplicated inline `request_counters` object in snapshot and
  delta payloads with calls to the helper.

## Isomorphism Card
- Inputs covered: `poll_payload` snapshot mode and delta mode callers in
  `tui_ws_state` tests, plus source inspection of both payload builders.
- Ordering preserved: yes. Snapshot still computes ring, next sequence, events,
  and top-level JSON fields in the same order. Delta still computes ring, events,
  `to_seq`, and top-level JSON fields in the same order. Both payload builders
  still capture `state.request_counters()` before ring/event reads.
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
- `crates/mcp-agent-mail-server/src/tui_ws_state.rs`: 318 lines before, 315
  lines after, net -3 lines.

## Fresh-Eyes Correction
- The initial helper extraction read `state.request_counters()` inside the
  helper, which moved that state read after ring/event reads in both payload
  builders.
- Corrected by restoring `let counters = state.request_counters();` at the top
  of `snapshot_payload` and `delta_payload`, then passing the captured
  `RequestCounters` into the helper. This preserves the original read timing
  while still removing the duplicated JSON object shape.

## Verification
- `cargo fmt --check`: passed.
- `git diff --check`: passed.
- `cargo test -p mcp-agent-mail-server tui_ws_state --locked`: blocked before
  compilation because Cargo reported the current `Cargo.lock` would need an
  update while `--locked` was set.
- `cargo test -p mcp-agent-mail-server tui_ws_state`: passed after resolving the
  package graph locally; the transient `Cargo.lock` update was manually removed
  because no dependency manifest changed in this pass.
- `cargo check -p mcp-agent-mail-server --all-targets --locked`: blocked on the
  same stale-lockfile condition.
- `cargo clippy -p mcp-agent-mail-server --all-targets --locked -- -D warnings`:
  blocked on the same stale-lockfile condition.
- `ubs crates/mcp-agent-mail-server/src/tui_ws_state.rs`: 0 critical findings;
  warning inventory only, with fmt/clippy/check/test-build subchecks clean in
  the scanner shadow workspace.

## Proof Limits
- Full workspace tests and clippy were not rerun for this narrow pass.
- The normal locked package check/clippy gates remain blocked until the
  repository's lockfile state is reconciled by the owner of that surface.
