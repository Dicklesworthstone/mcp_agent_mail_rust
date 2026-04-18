# SPEC (Deferred): Browser TUI Parity Contract

**Status**: deferred as of 2026-04-18 via `br-il53l.1`

## Intent

The original browser-parity plan was to ship a browser-loadable page at `/web-dashboard`
that mirrors the live terminal TUI in real time. The intended transport was HTTP polling
for state (`GET /mail/ws-state`) plus input ingress (`POST /mail/ws-input`), with a
browser-side renderer capable of drawing the same screen model the terminal TUI exposes.

This document is the recovery point for that idea after the explicit RETIRE decision on
2026-04-18. It is not a promise that the feature is returning soon.

## What Existed At Deferral Time

- `experimental/mcp-agent-mail-wasm/` existed as a standalone WASM/browser surface, but remained
  incomplete and under-verified.
- `crates/mcp-agent-mail-server/src/tui_ws_state.rs` and
  `crates/mcp-agent-mail-server/src/tui_web_dashboard.rs` contained the beginnings of the
  browser-state and web-dashboard paths.
- `README.md` still described `/mail/ws-state`, `/mail/ws-input`, and `/web-dashboard`
  as real shipped browser surfaces at the time of deferral.
- The server-rendered `/mail/*` web UI was real and remains supported; the deferred work
  concerns only the browser TUI mirror path.

## Why Deferred

The repo owner explicitly closed the ship-or-retire decision bead `br-il53l.1` as RETIRE
on 2026-04-18. The reasons captured in the backlog and reflected in project posture were:

- the browser TUI mirror was materially oversold relative to delivered code,
- there was no strong user pull justifying immediate completion,
- higher-leverage work was competing for attention,
- keeping the docs honest was better than shipping a brittle, half-wired surface.

The practical result is: the browser mirror is deferred, not silently abandoned, and the
project should stop implying that `/web-dashboard` or `/mail/ws-state` are maintained
production surfaces until a future reactivation effort lands.

## Architectural Seams Worth Preserving

These seams were considered reusable enough to preserve conceptually even though the full
feature was deferred:

- `/mail/*` and `/web-dashboard/*` are already namespaced separately in server routing.
- The TUI runtime already has internal state publication concepts that could feed a future
  browser renderer.
- Existing HTTP bearer-auth middleware can be reused for any future browser surface.
- The project already distinguishes the server-rendered `/mail/*` UI from any live browser
  mirror, which reduces future migration ambiguity.

## Future Revisit Checklist

If this feature is revived after significant codebase drift, start here:

1. Re-audit the current code state before trusting any prior README claims or TODOs.
2. Re-spec the state transport. Polling may still be acceptable, but SSE or WebSocket may
   be a better fit by then.
3. Re-evaluate the renderer target. A WASM canvas renderer was one candidate, not a
   binding decision.
4. Decide whether the right future surface is still a TUI mirror, versus a purpose-built
   web UI using `/mail/api/*` style endpoints.
5. Re-verify auth, rate limiting, and operator observability requirements from scratch.
6. Confirm whether `experimental/mcp-agent-mail-wasm/` still contains useful prior art or should
   be treated as archival reference only.

## What Needs Re-Validation Before Any Resurrection

- State snapshot and delta semantics for `/mail/ws-state`
- Input event shape and trust boundary for `/mail/ws-input`
- Browser auth flow and token reuse rules
- Runtime mode semantics such as `live`, `warming`, and `inactive`
- Accessibility, observability, and testability requirements for any browser surface

None of the above should be assumed correct merely because an earlier draft existed.

## Deferred Follow-On Beads

At deferral time, the RETIRE branch still included follow-on work to make the repo honest:

- `br-il53l.9` — remove the Browser State Sync section from `README.md`
- `br-il53l.10` — replace the Web Dashboard section in `README.md`
- `br-il53l.11` — make deferred browser endpoints return honest `501`
- `br-il53l.12` — park `mcp-agent-mail-wasm`
- `br-il53l.13` — clean up `AGENTS.md`
- `br-il53l.17` — changelog entry documenting the deferral

This spec should be read alongside those retirement tasks, not as a substitute for them.

## Current Guidance

- Treat `/mail/*` as the supported browser-facing surface.
- Treat `/web-dashboard`, `/mail/ws-state`, and `/mail/ws-input` as deferred browser-mirror
  concepts until the retirement beads complete and a future resurrection plan exists.
- If future work restarts this area, begin by opening a new audit bead rather than assuming
  this document is an implementation-ready spec.
