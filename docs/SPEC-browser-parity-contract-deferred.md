# Browser Dashboard Contracts: Public Replay Shipped, Live Parity Deferred

**Status**: privacy-bounded public replay shipped 2026-08-02; authenticated live
browser parity remains deferred via `br-il53l.1`

## What Shipped

The marketing-site dashboard is now the production Agent Mail `DashboardScreen`
compiled to WebAssembly and rendered by FrankenTUI's browser renderer. It is not a
DOM imitation and it is not a video: the browser host sends normalized keyboard and
pointer events into the same screen update logic, advances a deterministic replay
clock, and applies FrankenTUI flat cell patches to a canvas.

The portable boundary lives in `crates/mcp-agent-mail-dashboard-wasm/`, a standalone
nested workspace intentionally excluded from the native root workspace. Its default
browser dependency graph contains the shared TUI modules, FrankenTUI runtime/rendering
crates, serde, and browser bindings; it does not link the Agent Mail server, native DB
pool, mailbox storage, search index, or mutation tools.

The shipped public surface has a narrower contract than live parity:

- `tui_screens/dashboard.rs` is the exact production screen implementation.
- `TuiSharedState` is an in-memory browser adapter populated only by a validated pack.
- replay time and event order are host-driven and deterministic;
- reduced-motion mode disables chart transitions and holds a static frame;
- resize events enter the real responsive DashboardScreen layout;
- no HTTP bearer token, mailbox API, database URL, filesystem root, or write capability
  is present in the browser bundle.

## Public Data Contract

The checked-in `agent_mail.demo_pack.v1` combines two deliberately different classes
of data:

1. Six scalar aggregate counts are read from Agent Mail SQLite using count-only SQL:
   projects, agents, messages, active file reservations, contact links, and pending
   acknowledgements.
2. Every identifying detail is curated synthetic material: agent names, project names,
   paths, messages, subjects, thread IDs, programs, models, and replay events.

The exporter never serializes its source path. It resolves the source and output parent
directories once, requires distinct physical directories, and keeps using those resolved
paths so a later working-directory or parent-symlink change cannot redirect either read or
publication. It copies source database and WAL bytes into a verified owner-only private
snapshot without opening the source through SQLite; this preserves source contents and its
directory namespace, although ordinary raw reads may still update access-time metadata or
trigger filesystem audit events. Output is staged completely in the resolved destination
directory and published without clobbering an existing leaf. The destination directory must
be owned by the exporting user and not be group- or world-writable; protecting either
resolved parent directory from coordinated replacement by the same user or by any actor
with rename authority over an ancestor remains an operator trust boundary. A failed
post-publication file or directory sync is reported explicitly: the final path exists, but
its durability could not be confirmed.

Before publication, the typed pack validator recursively checks text and path fields,
bounded collections, finite metrics, monotonic actions, duration bounds, the privacy-policy
marker, and the pack content digest. The website then verifies the versioned artifact
manifest's byte counts and SHA-256 digests before initializing either WASM module.

Refresh a pack offline with:

```bash
cargo run \
  --manifest-path crates/mcp-agent-mail-dashboard-wasm/Cargo.toml \
  --locked \
  --no-default-features \
  --features exporter \
  --bin am-export-dashboard-demo -- \
  --source /absolute/path/to/storage.sqlite3 \
  --output /new/path/demo_pack.v1.json \
  --source-revision "$(git rev-parse HEAD)" \
  --captured-at "2026-08-02T00:00:00Z"
```

The exporter feature is native Unix-only and explicitly enables the shared browser
contracts whose DTOs the pack serializes; its SQLite, CLI, and filesystem dependencies are
excluded from the default WASM target graph. The output path must not already exist,
`source_revision` must be exactly 40 lowercase hexadecimal characters, and `captured_at`
must be RFC 3339 / ISO-8601. Its 30-second source I/O check is an elapsed budget between
filesystem operations, not a killable deadline for a blocked kernel or network-filesystem
call; use process-level supervision when reading an untrusted mount. Before replacing a
published pack, run the locked default and exporter matrices plus the website artifact
digest tests:

```bash
cargo test \
  --manifest-path crates/mcp-agent-mail-dashboard-wasm/Cargo.toml \
  --locked
cargo test \
  --manifest-path crates/mcp-agent-mail-dashboard-wasm/Cargo.toml \
  --locked \
  --no-default-features \
  --features exporter \
  --all-targets
```

## Intent

The original live browser-parity plan was to ship a browser-loadable page at `/web-dashboard`
that mirrors the live terminal TUI in real time. The intended transport was HTTP polling
for state (`GET /mail/ws-state`) plus input ingress (`POST /mail/ws-input`), with a
browser-side renderer capable of drawing the same screen model the terminal TUI exposes.

The public replay does not revive that authenticated transport. This document remains
the recovery point for live parity after the explicit RETIRE decision on 2026-04-18.

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
project should stop implying that `/web-dashboard` or `/mail/ws-input` are maintained
production surfaces until a future reactivation effort lands. `/mail/ws-state` remains a
supported polling endpoint for robot/TUI consumers, but that does **not** mean browser
parity shipped.

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

- State snapshot and delta semantics for browser consumption of `/mail/ws-state`
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
- Treat the marketing-site WASM dashboard as an interactive, read-only public replay of
  the production screen—not as a connection to the viewer's Agent Mail instance.
- Refresh public packs only through the count-only exporter and the typed privacy gate.
- Treat `/mail/ws-state` as a supported polling endpoint for robot/TUI consumers, not as
  proof that the browser mirror shipped.
- Treat `/web-dashboard` and `/mail/ws-input` as deferred browser-mirror concepts until a
  future resurrection plan exists.
- If future work restarts this area, begin by opening a new audit bead rather than assuming
  this document is an implementation-ready spec.
