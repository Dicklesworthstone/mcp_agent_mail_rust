# TUI v2 Parity + Differentiation Matrix (Legacy mail-web vs Rust v1 vs FrankentUI)

**Bead:** br-3vwi.1.2  
**Date:** 2026-02-10  
**Status:** Draft (in progress)

## Scope

This document is a capability matrix and decision log comparing:

- **Legacy mail-web**: the Python `/mail/*` web UI and related APIs
- **Current Rust surfaces (v1)**: existing Rust web UI and 8-screen TUI
- **FrankentUI showcase**: widget/pattern opportunities for a “showcase-grade” operator console

It is intentionally not an exhaustive parity contract. The exhaustive web parity
matrix/conformance contract is owned by `br-3vwi.13.1`.

## Sources (Concrete)

- Legacy Python routes/tests: `legacy_python_mcp_agent_mail_code/mcp_agent_mail/src/mcp_agent_mail/http.py`,
  `legacy_python_mcp_agent_mail_code/mcp_agent_mail/tests/test_mail_viewer_e2e.py`
- Current Rust web UI routes: `crates/mcp-agent-mail-server/src/mail_ui.rs`
- Current Rust TUI screen registry: `crates/mcp-agent-mail-server/src/tui_screens/mod.rs`
- V2 contract/IA: `docs/TUI_V2_CONTRACT.md`

## Legend

- **Yes**: implemented and operator-usable
- **Partial**: present but missing key behaviors (filtering, pagination, actions, safety)
- **No**: not implemented
- **V2**: explicitly targeted by the V2 contract

## Capability Matrix

| Capability | Legacy mail-web (Python) | Rust web UI (current) | Rust TUI (current) | FrankentUI Showcase Angle | V2 Decision | Owner Bead(s) |
|---|---|---|---|---|---|---|
| Unified inbox HTML (`/mail`, `/mail/unified-inbox`) | Yes | Partial (`/mail/unified-inbox` only; `/mail` is index) | No | Virtualized list + badges + preview dock | Close parity + add cross-project explorer | br-3vwi.5.1, br-3vwi.13.2 |
| Unified inbox JSON (`/mail/api/unified-inbox`) | Yes | No | No | N/A (API), but powers fast UIs | Close parity (API parity for web/export/tests) | br-3vwi.13.2, br-3vwi.13.8 |
| Projects list HTML (`/mail/projects`) | Yes | Partial (project list is `/mail`) | No | Table + sort + stats badges | Intentional delta OK, but ensure deep-links + stats parity | br-3vwi.13.2, br-3vwi.5.1 |
| Project detail (`/mail/{project}`) | Yes | Yes | Partial (project-scoping exists, but UI is TUI-centric) | Table + drilldowns + scoped dashboards | Keep + expand (projects screen in TUI) | br-3vwi.5, br-3vwi.13.2 |
| Agent inbox HTML (`/mail/{project}/inbox/{agent}`) | Yes | Yes | Partial (Messages screen can browse/search; per-agent “inbox” lens varies) | List + dock preview + quick actions | Close “agent inbox lens” parity across TUI/web | br-3vwi.5.1, br-3vwi.5.3 |
| Mark read (single) (`POST …/mark-read`) | Yes | No | Partial (tool exists; UI action may not) | Context action in list/detail | Close parity (web + TUI action) | br-3vwi.5.3, br-3vwi.13.2 |
| Mark all read (`POST …/mark-all-read`) | Yes | No | Partial | Bulk action + confirmation modal | Close parity (web + TUI bulk) | br-3vwi.5.3, br-3vwi.13.2 |
| Message detail HTML (`/mail/{project}/message/{mid}`) | Yes | Yes | Yes (detail dock/inspector path) | Rich markdown + code highlighting | Keep + improve markdown fidelity | br-3vwi.3, br-3vwi.13.3 |
| Thread detail HTML (`/mail/{project}/thread/{thread_id}`) | Yes | Yes | Yes | Conversation view + participant chips | Keep + add ack status/participants | br-3vwi.5.2, br-3vwi.6 |
| Search page (`/mail/{project}/search?q=`) | Yes | Yes | Yes (FTS in Messages) | Facets + snippets + preview pane | Expand to Search Cockpit (facets/snippets) | br-3vwi.4, br-3vwi.2 |
| XSS escape in search query/path | Yes (tests) | Partial (depends on template + sanitizer) | N/A | Safe markdown + HTML escaping | Must match legacy safety guarantees | br-3vwi.13.7, br-3vwi.10.14 |
| File reservations view (`/mail/{project}/file_reservations`) | Yes | Yes | Yes | Conflict highlighting + TTL countdown | Keep + upgrade to board-style UX | br-3vwi.5.3, br-3vwi.6.2 |
| Attachments browser (`/mail/{project}/attachments`) | Yes | Yes | Partial (view exists via tools/resources; UI varies) | Inline preview + provenance trails | Keep + add inline preview + export parity | br-3vwi.5.4, br-3vwi.13.5 |
| Overseer compose (`/mail/{project}/overseer/compose`) | Yes | Yes | No | Guided form + validation | Keep (web-only OK), but ensure “send” parity | br-3vwi.13.2 |
| Overseer send (`POST /mail/{project}/overseer/send`) | Yes | No | No | Action + audit log | Close parity (web) | br-3vwi.13.2 |
| Locks API (`/mail/api/locks`) | Yes | Yes | N/A | N/A | Keep parity | br-3vwi.13.2 |
| Archive browser / time-travel (`/mail/archive/*`) | Yes | No | No | Timeline + graph + snapshot explorer | Decide: migrate to “Exports” + static export, or re-add archive web UI | br-3vwi.13.5, br-3vwi.13.6, br-3vwi.6 |
| Web “projects siblings” ops (`POST /mail/api/projects/.../siblings/...`) | Yes | No | No | N/A | Likely intentional delta unless needed for ops | TBD (create bead if needed) |
| TUI Dashboard (stats + live stream) | No | Partial (web index/unified inbox) | Yes | Dense stat grid + alert badges | Differentiate: TUI-first operations console | br-3vwi.6.5, br-3vwi.7 |
| TUI Tool Metrics | No | No | Yes | Percentiles + histograms + heatmaps | Differentiate: stronger than web | br-3vwi.7.5 |
| TUI System Health | No | Partial (web has minimal) | Yes | Drill-down panels + sparklines | Differentiate | br-3vwi.7.5 |
| TUI Timeline + Inspector | Partial (archive/timeline) | No | Yes | Virtualized rail + typed filters + JSON view | Differentiate, but preserve deep-link parity | br-3vwi.5.5, br-3vwi.10.3 |

## Recommendations (Immediate)

1. Treat `docs/TUI_V2_CONTRACT.md` as the single product contract source; this matrix should link into it and feed `br-3vwi.13.1`.
2. Close the biggest user-visible gaps first:
   - unified inbox semantics (`/mail` vs `/mail/unified-inbox`)
   - web “mark read” actions
   - overseer send endpoint
   - archive/time-travel story (explicitly fold into export/static pipeline or restore web UI)

## Quality Checks

- Every “Close parity” row must have an owner bead ID before implementation starts.
- When a row is implemented, link evidence (tests/fixtures/E2E script) in the owner bead.
- If a parity row is intentionally skipped, document the delta in `docs/TUI_V2_CONTRACT.md` (or the web parity contract).

