# TUI V2 Product Contract

> Canonical contract for AgentMailTUI v2 (br-3vwi epic).
> Every downstream track references this document for screen IA, entity model,
> navigation graph, operator journeys, layout principles, and done criteria.

**Beads:** br-3vwi.1, br-3vwi.1.3  
**Last updated:** 2026-02-10  
**Status:** Published (controlling artifact)

---

## 1. Vocabulary

| Term | Definition |
|------|-----------|
| **Project** | A directory-scoped workspace (`ProjectRow.slug`). Root coordination unit. |
| **Agent** | A registered coding-agent identity (`AgentRow.name`). Uniquely identified by adjective+noun pair within a project. |
| **Message** | A Markdown body addressed to one or more agents (`InboxRow`). Belongs to a thread. |
| **Thread** | A conversation keyed by `thread_id` string. Groups related messages chronologically. |
| **Reservation** | An advisory file-path lease (`FileReservationRow`). Exclusive or shared, with TTL. |
| **Contact** | A peer-to-peer trust link between agents (`AgentLinkRow`). Status: pending / approved / blocked. |
| **Tool** | One of 34 MCP tool implementations. Tracked by call count, error count, and latency histogram. |
| **Metric** | A `MetricsSnapshotEntry` combining call/error/latency data per tool. |
| **Event** | A `MailEvent` from the ring buffer (tool calls, HTTP requests, lifecycle, health pulses). |
| **Product** | A cross-project grouping for federated search and inbox aggregation. |

---

## 2. Screen Information Architecture

### 2.1 V2 Screen Registry (12 screens)

V2 expands from 8 to 12 screens, filling placeholders and adding new capabilities.

| # | ID | Label | Category | Status | Description |
|---|-----|-------|----------|--------|-------------|
| 1 | `Dashboard` | Dash | Overview | **Enhance** | Operational overview: stat grid, event stream, sparklines, alert badges |
| 2 | `Messages` | Msg | Communication | **Enhance** | Message browser with FTS search, filtering, GFM preview in detail pane |
| 3 | `Threads` | Threads | Communication | **Enhance** | Thread explorer with conversation view, participant list, ack status |
| 4 | `Agents` | Agents | Operations | **Build** | Agent roster: name, program, model, last active, inbox stats, contacts |
| 5 | `Reservations` | Reserv | Operations | **Build** | Reservation board: active/expired/conflicts, TTL countdown, agent grouping |
| 6 | `ToolMetrics` | Tools | System | **Build** | Per-tool metrics: call count, error rate, P50/P95/P99 latency, slow flags |
| 7 | `SystemHealth` | Health | System | **Enhance** | DB/queue/pool diagnostics, circuit breakers, WBQ/coalescer health |
| 8 | `Timeline` | Time | Overview | **Enhance** | Chronological event timeline with severity/source/kind filtering + inspector |
| 9 | `Projects` | Proj | Overview | **New** | Project browser: slug, human_key, agent count, message count, reservation count |
| 10 | `Contacts` | Links | Communication | **New** | Cross-agent contact graph: approved/pending/blocked links, policy display |
| 11 | `Search` | Search | Communication | **New** | Unified cross-project search with faceted filtering (agent, thread, date range) |
| 12 | `Exports` | Export | Operations | **New** | Share/export status: snapshot history, bundle artifacts, hosting targets |

### 2.2 Screen Categories

| Category | Screens | Purpose |
|----------|---------|---------|
| **Overview** | Dashboard, Timeline, Projects | Operational situational awareness |
| **Communication** | Messages, Threads, Contacts, Search | Message and thread exploration |
| **Operations** | Agents, Reservations, Exports | Agent and resource management |
| **System** | ToolMetrics, SystemHealth | Infrastructure diagnostics |

### 2.3 Navigation Graph

```
                    ┌─────────────┐
                    │  Tab Bar    │ ← 1-9/0 number keys, Tab/BackTab
                    └──────┬──────┘
                           │
    ┌──────┬──────┬────────┼────────┬──────┬──────┐
    ▼      ▼      ▼        ▼        ▼      ▼      ▼
  Dash   Msg   Threads  Agents  Reserv  Tools  Health ...
    │      │      │        │        │      │
    │      │      │        │        │      └─► ToolByName deep-link
    │      │      │        │        └─► agent grouping → Agents deep-link
    │      │      │        └─► agent name → Messages/Threads deep-link
    │      │      └─► thread → Messages deep-link (message detail)
    │      └─► message.thread_id → Threads deep-link
    └─► event → Timeline deep-link (TimelineAtTime)

    Command Palette (Ctrl+P / :)
    ├── Screen navigation (all 12 screens)
    ├── Dynamic: agents (top 50 by activity)
    ├── Dynamic: threads (top 50 by recency)
    ├── Dynamic: tools (top 50 by calls)
    └── Quick actions (context from focused entity)
```

### 2.4 Deep-Link Targets (V2)

| Target | Status | Source screens | Destination |
|--------|--------|---------------|-------------|
| `TimelineAtTime(i64)` | Implemented (v1) | Dashboard event click | Timeline, cursor at timestamp |
| `MessageById(i64)` | Implemented (v1) | Threads, Search, Timeline | Messages, detail pane open |
| `ThreadById(String)` | Implemented (v1) | Messages, Dashboard, Agents | Threads, conversation view |
| `AgentByName(String)` | Implemented (v1) | Messages, Threads, Reservations | Agents, selected agent |
| `ToolByName(String)` | Implemented (v1) | Dashboard, Timeline | ToolMetrics, selected tool |
| `ProjectBySlug(String)` | Partial (v1) | Search, Contacts | Projects/Dashboard scoped to project |
| `ReservationByAgent(String)` | Implemented (v1) | Agents, Timeline | Reservations, select agent reservation |
| `ReservationById(i64)` | Planned (v2) | Agents, Timeline | Reservations, selected reservation |
| `ContactByPair(String,String)` | Planned (v2) | Agents | Contacts, selected link |

### 2.5 Web Route Scheme (Baseline)

The Rust HTTP server already exposes a web UI under `/mail/*`. V2 deep-link
copying should preserve this shape (or evolve it with explicit, intentional
deltas) so links round-trip across **TUI ↔ web UI ↔ static export**.

Legacy Python -> Rust web parity contract (route-by-route + CI guard):
- `docs/SPEC-web-ui-parity-contract.md` (br-3vwi.13.1)

Current baseline routes (see `crates/mcp-agent-mail-server/src/mail_ui.rs`):

- `/mail` (project index)
- `/mail/unified-inbox` (cross-project inbox view)
- `/mail/{project_slug}` (project detail)
- `/mail/{project_slug}/inbox/{agent_name}`
- `/mail/{project_slug}/message/{message_id}`
- `/mail/{project_slug}/thread/{thread_id}`
- `/mail/{project_slug}/search`
- `/mail/{project_slug}/file_reservations`
- `/mail/{project_slug}/attachments`
- `/mail/{project_slug}/overseer/compose` (human overseer compose)

Static export should mirror these routes as filesystem paths/anchors when
possible so “copy link” remains stable across hosting targets.

---

## 3. Entity Model

### 3.1 Primary Entities and Data Sources

| Entity | DB Source | Cache | Refresh |
|--------|----------|-------|---------|
| Project | `list_projects()` | DbStatSnapshot | DbPoller 2s |
| Agent | `list_agents(project_id)` | DbStatSnapshot.agents_list | DbPoller 2s |
| Message | `search_messages_fts()`, `list_recent_messages()` | None (on-demand) | Screen tick |
| Thread | `list_threads()`, `get_thread_messages()` | None (on-demand) | Screen tick 5s |
| Reservation | `list_file_reservations(active_only)` | None (on-demand) | Screen tick 2s |
| Contact | `list_contacts(project_id, agent_id)` | None (on-demand) | Screen tick 5s |
| ToolMetric | `tool_metrics_snapshot_full()` | Atomic counters | Always fresh |
| Event | `EventRingBuffer.events_since(seq)` | Ring buffer 10k | Push-driven |
| InboxStats | `get_inbox_stats(agent_id)` | 30s scoped cache | On access |

### 3.2 Relationships

```
Project 1──* Agent
Project 1──* Message
Project 1──* FileReservation
Agent   1──* Message (as sender)
Agent   1──* Message (as recipient, via inbox)
Agent   1──* FileReservation
Agent   *──* Agent (via AgentLink / contacts)
Message *──1 Thread (via thread_id)
Tool    1──1 MetricsSnapshotEntry (in-memory atomic)
```

---

## 4. Operator Journeys

### 4.1 Incident Triage (P0)

**Goal:** Operator sees an anomaly and tracks it to root cause within 30 seconds.

1. **Dashboard** shows error badge (red counter) or elevated error sparkline
2. Operator presses `8` or clicks → **Timeline** with severity filter set to `Error`
3. Timeline cursor on the error event → inspector pane shows full payload
4. Inspector shows `tool_name` → deep-link to **ToolMetrics** (`ToolByName`)
5. ToolMetrics shows P95 spike + error count → identifies the slow/failing tool
6. Alternatively: inspector shows `agent_name` → deep-link to **Agents** → see agent inbox, recent messages

**Key bindings in flow:** `8` (Timeline), `e` (filter Error), `Enter` (inspect), `Ctrl+P` → "View tool" (deep-link)

### 4.2 Inbox Zero (P0)

**Goal:** Operator verifies all agents have empty inboxes / no stale unread messages.

1. **Dashboard** shows per-agent unread counts in stat grid
2. Press `4` → **Agents** screen, sorted by `unread_count` descending
3. Select agent with unread > 0 → deep-link to **Messages** filtered by agent
4. **Messages** screen shows unread messages with FTS search
5. Inspect message → GFM body preview in dock panel
6. Optionally deep-link to thread for full conversation context

**Key bindings:** `4` (Agents), `Enter` (select), `2` (Messages), `/` (search)

### 4.3 Ack SLA Enforcement (P0)

**Goal:** Identify messages requiring acknowledgment that are overdue.

1. **Dashboard** shows `ack_pending` counter (from DbStatSnapshot)
2. Press `2` → **Messages** with preset filter `Ack Required`
3. Messages list shows ack-required messages sorted by age (oldest first)
4. Select message → inspect → see `ack_required: true`, sent timestamp, recipient
5. Deep-link to agent → verify agent is still active (`last_active_ts`)
6. Deep-link to thread → see if conversation is stalled

**Key bindings:** `2` (Messages), `a` (ack-required preset), `j/k` (navigate), `Enter` (inspect)

### 4.4 Conflict Diagnosis (P1)

**Goal:** Understand and resolve file reservation conflicts between agents.

1. **Dashboard** shows reservation count + potential conflict badge
2. Press `5` → **Reservations** screen
3. List shows all active reservations grouped by path pattern
4. Conflicting reservations highlighted (overlapping exclusive patterns)
5. Select conflict → see both agents, TTL remaining, reason (bead ID)
6. Deep-link to agent → verify activity
7. Optionally force-release stale reservation from inspector actions

**Key bindings:** `5` (Reservations), `c` (show conflicts only), `Enter` (inspect), `F` (force-release)

### 4.5 Cross-Project Discovery (P1)

**Goal:** Find messages/threads across multiple projects (product-level view).

1. Press `Ctrl+P` → type project name → navigate to **Projects** screen
2. **Projects** lists all registered projects with stats
3. Select project → deep-link to **Dashboard** scoped to that project
4. Alternatively: press `0` → **Search** screen for cross-project FTS
5. Search results grouped by project, filterable by agent/thread/date
6. Select result → deep-link to **Messages** or **Threads** in target project

**Key bindings:** `9` (Projects), `0` (Search), `/` (query bar), `Enter` (select)

---

## 5. Screen Specifications

### 5.1 Dashboard (Enhance)

**Current:** Event log + stat grid + sparkline.

**V2 additions:**
- Per-agent activity indicators (dot: green=active <60s, yellow=idle <5m, gray=stale)
- Alert badges: ack overdue count, reservation conflict count, error rate
- Project selector dropdown (if multi-project)
- Clickable stat cards → deep-link to relevant screen

**Data sources:** `DbStatSnapshot`, `EventRingBuffer`, `tool_metrics_snapshot()`

**Layout (responsive):**
- `<80 cols:` Stacked: stats (3 rows), event log (fill)
- `80-119 cols:` Stats bar (1 row) + event log (fill) + sparkline (3 rows)
- `120+ cols:` Left: stats grid (30 cols) | Right: event log (fill) + sparkline (6 rows)

### 5.2 Messages (Enhance)

**Current:** FTS search + results list + detail dock pane.

**V2 additions:**
- GFM markdown rendering in detail pane (using `ftui_extras::syntax`)
- Message body preview with code highlighting
- Preset query bar: All / Urgent / Ack-Required / Unread / By-Agent
- Date range filtering
- Attachment indicator badges

**Data sources:** `search_messages_fts()`, `list_recent_messages()`, `get_inbox_stats()`

**Layout:** Two-pane DockLayout (primary: search+list, dock: detail). Dock position configurable.

### 5.3 Threads (Enhance)

**Current:** Thread list + conversation view (two-pane split).

**V2 additions:**
- Participant avatars (first letter + color) in conversation view
- Ack status per message (checkmark icon)
- Thread summary header (participant count, message count, last activity)
- Unread indicator on thread list entries

**Data sources:** `list_threads()`, `get_thread_messages()`, `get_inbox_stats()`

**Layout:** Left (40%): thread list | Right (60%): conversation. Adjustable via dock drag.

### 5.4 Agents (Build) -- Currently Placeholder

**Layout:**
- Primary: Agent table (name, program, model, last_active, unread, ack_pending, reservations)
- Dock: Agent detail (full whois info, recent commits, contacts, inbox stats)

**Columns:**
| Column | Source | Width |
|--------|--------|-------|
| Name | `AgentRow.name` | 16 |
| Program | `AgentRow.program` | 12 |
| Model | `AgentRow.model` | 16 |
| Last Active | `AgentRow.last_active_ts` | 12 (relative: "2m ago") |
| Unread | `InboxStatsRow.unread_count` | 6 |
| Ack Pending | `InboxStatsRow.ack_pending_count` | 6 |
| Reservations | count from `list_file_reservations` | 6 |

**Sort:** Default by `last_active_ts` descending. Toggle: `s` cycles sort column.

**Detail pane:** Whois-style info with recent git commits (if `include_recent_commits=true`).

**Deep-links in:** `AgentByName(String)` → select and scroll to agent.
**Deep-links out:** Click agent → Messages (filtered by agent), Threads (filtered by agent), Reservations (filtered by agent).

**Keybindings:** `j/k` navigate, `Enter` toggle detail, `s` cycle sort, `/` filter by name.

**Data sources:** `list_agents()`, `get_inbox_stats()`, `list_file_reservations()`, `whois()`

### 5.5 Reservations (Build) -- Currently Placeholder

**Layout:**
- Primary: Reservation table (path, agent, exclusive, reason, TTL remaining, status)
- Dock: Reservation detail (full row data, conflict analysis, agent link)

**Columns:**
| Column | Source | Width |
|--------|--------|-------|
| Path | `FileReservationRow.path_pattern` | 30 (truncate) |
| Agent | agent name (join) | 16 |
| Excl | `exclusive` flag | 4 (icon) |
| Reason | `reason` | 20 (truncate) |
| TTL | `expires_ts - now` | 8 (countdown: "47m left") |
| Status | derived | 8 (Active/Expired/Released) |

**Filters:**
- `a` = Active only (default)
- `c` = Conflicts only (overlapping exclusive patterns)
- `x` = Expired (for history)
- `r` = Released

**Conflict highlighting:** Rows with overlapping exclusive path patterns get amber background.

**TTL countdown:** Updated every tick (100ms), color shifts: green >30m, yellow 5-30m, red <5m.

**Deep-links in:** `ReservationById(i64)` → select reservation.
**Deep-links out:** Agent name → Agents screen. Reason (bead ID) → external reference.

**Keybindings:** `j/k` navigate, `Enter` toggle detail, `a/c/x/r` filter, `F` force-release (with confirmation), `s` cycle sort.

**Data sources:** `list_file_reservations(active_only)`, `get_active_reservations()`, agent name join via `get_agent_by_id()`

### 5.6 ToolMetrics (Build) -- Currently Placeholder

**Layout:**
- Primary: Tool metrics table (name, cluster, calls, errors, error%, P50, P95, P99, slow flag)
- Dock: Tool detail (capabilities, complexity, latency histogram visualization)

**Columns:**
| Column | Source | Width |
|--------|--------|-------|
| Tool | `MetricsSnapshotEntry.name` | 28 |
| Cluster | `cluster` | 14 |
| Calls | `calls` | 8 (right-aligned) |
| Errors | `errors` | 6 (right-aligned, red if >0) |
| Err% | `errors/calls * 100` | 6 |
| P50 | `latency.p50_ms` | 8 (ms) |
| P95 | `latency.p95_ms` | 8 (ms, amber if >200ms) |
| P99 | `latency.p99_ms` | 8 (ms, red if >500ms) |
| Slow | `latency.is_slow` | 4 (icon) |

**Sort:** Default by `calls` descending. Toggle: `s` cycles through columns.

**Slow tool highlighting:** Tools with `is_slow=true` get red row background.

**Sparkline:** Per-tool latency trend (if we add per-tool history tracking).

**Deep-links in:** `ToolByName(String)` → select tool.
**Deep-links out:** None directly (tools are leaf entities).

**Keybindings:** `j/k` navigate, `Enter` toggle detail, `s` cycle sort, `z` toggle zero-call tools, `R` reset metrics.

**Data sources:** `tool_metrics_snapshot_full()` (always fresh, atomic counters)

### 5.7 SystemHealth (Enhance)

**Current:** DB diagnostics, queue depth, connection pool.

**V2 additions:**
- WBQ health panel (depth, capacity, peak, over-80% timer, backpressure count)
- Commit coalescer health (pending, soft_cap, peak, shards)
- Circuit breaker status indicators
- Historical trend sparklines for key metrics

**Data sources:** `DbStatSnapshot`, `TuiSharedState` counters, `health_check()` output

### 5.8 Timeline (Enhance)

**Current:** Event list with filtering + inspector dock.

**V2 additions:**
- Source filter chips (toggleable: Tooling, Http, Mail, Reservations, Lifecycle, Database)
- Kind filter chips (toggleable per MailEventKind)
- Multi-select for batch inspection
- Event export (copy to clipboard as JSON)

### 5.9 Projects (New)

**Layout:** Table + detail dock.

**Columns:** Slug, human_key, agents, messages, reservations, created.

**Data sources:** `list_projects()`, `DbStatSnapshot` per-project.

### 5.10 Contacts (New)

**Layout:** Table + detail dock.

**Columns:** From agent, To agent, Status, Reason, Updated, Expires.

**Data sources:** `list_contacts()` per agent.

### 5.11 Search (New)

**Layout:** Query bar + facet rail (left 20%) + results list + preview dock.

**Facets:** Project, Agent, Date range, Thread, Importance.

**Data sources:** `search_messages_fts()`, `search_messages_product()` for cross-project.

### 5.12 Exports (New)

**Layout:** Export history table + status badges.

**Data sources:** Share/export module output, bundle manifest.

---

## 6. Reactive Layout Principles

### 6.1 Breakpoints

| Tier | Width | Behavior |
|------|-------|----------|
| **Xs** | <60 cols | Single column, no dock pane, condensed stats |
| **Sm** | 60-89 | Single column with collapsible dock (toggled) |
| **Md** | 90-119 | Two-column with dock (default 40%) |
| **Lg** | 120-159 | Two-column with wider dock, stat sidebar |
| **Xl** | 160+ | Three-column: sidebar + content + dock |

### 6.2 Layout Rules

1. **Dock panes auto-hide below Sm.** User can toggle with `d` key.
2. **Stat grids collapse to single-line summary below Md.**
3. **Table columns drop right-to-left** as width shrinks (priority columns stay).
4. **Sparklines require minimum 20 cols.** Below that, show numeric-only.
5. **Tab bar is always visible** (1 row fixed at top).
6. **Status line is always visible** (1 row fixed at bottom).

### 6.3 Constraint-Based Layout (ftui Flex)

All screen layouts use `ftui::layout::Flex` with `Constraint` variants:
- `Fixed(n)` for chrome (tab bar, status line)
- `Fill` for primary content area
- `Percentage(n)` or `Ratio(a,b)` for dock splits
- `Min(n)` for minimum viable pane sizes

### 6.4 DockLayout Enhancements

Current `DockLayout` supports Right/Left/Top/Bottom with ratio 20-80%.
V2 additions:
- Auto-collapse below `Min` threshold
- Remember position per-screen in persistence layer
- Double-click border to reset to default ratio

---

## 7. FrankentUI Widget Mapping

### 7.1 Widget Usage Plan

| Widget | Screens | Purpose |
|--------|---------|---------|
| `Block` | All | Borders, titles, section framing |
| `Paragraph` | All | Text display, status messages |
| `Table` | Agents, Reservations, ToolMetrics, Projects, Contacts | Tabular data with selection |
| `List` | Messages (results), Threads (list) | Scrollable item lists |
| `VirtualizedList` | Timeline, Messages (large results) | High-perf large lists |
| `TextInput` | Messages, Search | Query/filter input |
| `CommandPalette` | Global | Fuzzy search navigation |
| `NotificationStack` | Global | Toast notifications |
| `Sparkline` | Dashboard, ToolMetrics, SystemHealth | Trend visualization |
| `MiniBar` | Dashboard (stats), SystemHealth (queue depth) | Compact progress |
| `Badge` | Dashboard (alerts), Reservations (status) | Status pills |
| `Tabs` | Search (facets), possibly screen categories | Tab switching |
| `Scrollbar` | All list screens | Scroll position indicator |
| `StatusLine` | Global | Context hints, mode display |
| `Tree` | Contacts (agent graph), Threads (nested) | Hierarchical display |
| `JsonView` | Inspector (event payload) | JSON tree exploration |
| `BarChart` | ToolMetrics (latency comparison) | Visual comparison |
| `Heatmap` | Search (match density) | Density visualization |
| `MarkdownRenderer` | Messages (body preview) | GFM rendering |
| `SyntaxHighlighter` | Inspector (code in messages) | Code highlighting |
| `Spinner` | Loading states | Async operation feedback |
| `Modal` | Force-release confirmation | Destructive action gate |

### 7.2 Layout Primitives

| Primitive | Usage |
|-----------|-------|
| `Flex::vertical` | Chrome shell (tab bar / content / status line) |
| `Flex::horizontal` | Two-pane splits (list + detail) |
| `ResponsiveLayout` | Breakpoint-driven layout switching |
| `Constraint::Fixed` | Chrome rows, column widths |
| `Constraint::Fill` | Primary content areas |
| `Constraint::Min` | Minimum viable pane sizes |
| `Constraint::Percentage` | Dock ratios |

---

## 8. Global Keybinding Map

### 8.1 Global (Always Active)

| Key | Action |
|-----|--------|
| `?` | Toggle help overlay |
| `q` | Quit application |
| `Ctrl+P` / `:` | Open command palette |
| `Tab` / `BackTab` | Next / previous screen |
| `1`-`9`, `0` | Jump to screen by number |
| `m` | Toggle MCP/API transport |
| `T` | Cycle theme |
| `d` | Toggle dock pane visibility |
| `Ctrl+R` | Force refresh current screen data |

### 8.2 List Navigation (Active on List Screens)

| Key | Action |
|-----|--------|
| `j` / `Down` | Move cursor down |
| `k` / `Up` | Move cursor up |
| `g` / `Home` | Jump to top |
| `G` / `End` | Jump to bottom |
| `PgUp` / `PgDn` | Page up / page down |
| `Enter` | Open detail / toggle dock |
| `/` | Focus search/filter input |
| `Esc` | Clear search / close dock |

### 8.3 Screen-Specific

| Screen | Key | Action |
|--------|-----|--------|
| Dashboard | `f` | Toggle auto-follow |
| Messages | `a` | Ack-required preset |
| Messages | `u` | Unread preset |
| Reservations | `c` | Conflicts-only filter |
| Reservations | `F` | Force-release selected |
| ToolMetrics | `z` | Toggle zero-call tools |
| ToolMetrics | `R` | Reset metrics (with confirmation) |
| Timeline | `e` | Filter errors only |
| Timeline | `f` | Toggle auto-follow |
| Agents | `s` | Cycle sort column |
| All tables | `s` | Cycle sort column |

---

## 9. Priority and Implementation Order

### Phase 1: Fill Placeholders (P0)

Complete the three placeholder screens that already have screen IDs, tab slots, and data sources ready.

| Bead | Screen | Effort | Data Ready |
|------|--------|--------|------------|
| br-3vwi.4 subset | **Agents** | Medium | `list_agents()`, `get_inbox_stats()`, `whois()` |
| br-3vwi.5 subset | **Reservations** | Medium | `list_file_reservations()`, `get_active_reservations()` |
| br-3vwi.7 subset | **ToolMetrics** | Low | `tool_metrics_snapshot_full()` (always fresh) |

### Phase 2: Enhance Existing Screens (P0)

| Screen | Enhancement | Effort |
|--------|------------|--------|
| Dashboard | Alert badges, per-agent activity dots | Low |
| Messages | GFM preview, preset filters | Medium |
| Threads | Ack status, participant display | Low |
| Timeline | Source/kind filter chips | Low |
| SystemHealth | WBQ/coalescer panels | Low |

### Phase 3: New Screens (P1)

| Screen | Effort | Dependency |
|--------|--------|------------|
| Projects | Low | None (simple table) |
| Contacts | Medium | Agent screen (for deep-links) |
| Search | High | FTS, cross-project queries |
| Exports | Medium | Share module integration |

### Phase 4: Advanced Features (P2)

- Rich GFM rendering pipeline (br-3vwi.3)
- Search cockpit with facets (br-3vwi.4)
- Operator macro recorder (br-3vwi.8.4)
- Performance hardening (br-3vwi.9)
- Comprehensive test matrix (br-3vwi.10)

---

## 10. Success Criteria

### 10.1 Functional

| Criterion | Measurement |
|-----------|-------------|
| All 12 screens render without panic | Snapshot test per screen at each breakpoint |
| All deep-links navigate correctly | E2E test for each DeepLinkTarget variant |
| Placeholder screens replaced with functional UI | Zero PlaceholderScreen instances |
| Responsive layout adapts at all 5 breakpoints | PTY test at 40x10, 80x24, 120x40, 160x50, 200x60 |
| Command palette finds all entities | Test: agents, threads, tools all appear in palette |
| Keybindings work per screen | Unit test per screen keybinding handler |

### 10.2 Performance

| Criterion | Target |
|-----------|--------|
| Frame render time | <16ms at 80x24 (60fps capable) |
| Screen switch latency | <50ms (perceived instant) |
| Search query to results | <200ms for FTS, <500ms for LIKE fallback |
| Event ring ingestion | <1ms per event push |
| Memory (12 screens loaded) | <50MB RSS |

### 10.3 Quality

| Criterion | Target |
|-----------|--------|
| Clippy warnings | 0 (workspace-wide) |
| Test coverage (TUI crate) | >80% line coverage |
| Snapshot tests | 1 per screen per breakpoint (5 breakpoints x 12 screens = 60) |
| E2E assertions | >150 across all TUI test scripts |

---

## 11. Non-Goals

- **Mobile / touch optimization.** Terminal-only; mouse is secondary to keyboard.
- **Theming API for end users.** Internal palette only; operator does not customize colors.
- **Plugin system for custom screens.** All screens are compiled-in.
- **Real-time WebSocket push.** Event ring is push-driven internally; no external WebSocket.
- **Multi-user authentication in TUI.** TUI is single-operator; auth is HTTP-level only.
- **Undo/redo for destructive actions.** Force-release and metric reset are one-way with confirmation modal.

---

## 12. Parity Matrix: V1 TUI to V2

Related artifacts:

- `docs/TUI_V2_PARITY_DIFF.md` (br-3vwi.1.2): legacy mail-web vs current Rust surfaces vs FrankentUI showcase analysis

| V1 Feature | V1 Status | V2 Target | Notes |
|------------|-----------|-----------|-------|
| Dashboard event log | Done | Enhance (alert badges) | Add per-agent dots, clickable stats |
| Dashboard sparkline | Done | Keep | Add per-tool sparklines in ToolMetrics |
| Message FTS search | Done | Enhance (GFM preview) | Add markdown rendering in dock |
| Thread conversation | Done | Enhance (ack status) | Show checkmarks for acked messages |
| Agent roster | Placeholder | Build | Full table + detail pane |
| Reservation board | Placeholder | Build | Conflict highlighting, TTL countdown |
| Tool metrics | Placeholder | Build | Table + latency bars |
| System health | Done | Enhance (queue panels) | WBQ + coalescer health |
| Timeline + inspector | Done | Enhance (filter chips) | Source/kind toggle chips |
| Command palette | Done | Enhance (more dynamic) | Add project, reservation entities |
| Toast notifications | Done | Keep | No changes needed |
| Help overlay | Done | Enhance (context-aware) | Prioritize common keys |
| Theme cycling | Done | Keep | No changes needed |
| Transport toggle | Done | Keep | No changes needed |
| Layout persistence | Done | Enhance (per-screen) | Store dock position per screen |
| Projects screen | N/A | New | Simple table |
| Contacts screen | N/A | New | Agent link graph |
| Unified search | N/A | New | Cross-project FTS |
| Export status | N/A | New | Share module dashboard |

---

## Appendix A: Data Source Quick Reference

```rust
// Agents screen
list_agents(cx, pool, project_id) -> Vec<AgentRow>
get_inbox_stats(cx, pool, agent_id) -> Option<InboxStatsRow>
whois(ctx, project_key, agent_name, ...) -> WhoisResponse

// Reservations screen
list_file_reservations(cx, pool, project_id, active_only) -> Vec<FileReservationRow>
get_active_reservations(cx, pool, project_id) -> Vec<FileReservationRow>

// ToolMetrics screen
tool_metrics_snapshot_full() -> Vec<MetricsSnapshotEntry>  // always fresh, atomic

// Projects screen
list_projects(cx, pool) -> Vec<ProjectRow>

// Contacts screen
list_contacts(cx, pool, project_id, agent_id) -> (Vec<AgentLinkRow>, Vec<AgentLinkRow>)

// Search screen
search_messages_fts(cx, pool, project_id, query, limit) -> Vec<InboxRow>
search_messages_product(...) -> Vec<InboxRow>  // cross-project

// Shared state (always available)
TuiSharedState.db_stats_snapshot() -> Option<DbStatSnapshot>
TuiSharedState.events_since(seq) -> Vec<MailEvent>
TuiSharedState.sparkline_data() -> VecDeque<f64>
```

## Appendix B: File Locations

| Component | Path |
|-----------|------|
| Screen trait + registry | `crates/mcp-agent-mail-server/src/tui_screens/mod.rs` |
| App model (event loop) | `crates/mcp-agent-mail-server/src/tui_app.rs` |
| Shared state | `crates/mcp-agent-mail-server/src/tui_bridge.rs` |
| Chrome (tab bar, status) | `crates/mcp-agent-mail-server/src/tui_chrome.rs` |
| Dock layout | `crates/mcp-agent-mail-server/src/tui_layout.rs` |
| Events + severity | `crates/mcp-agent-mail-server/src/tui_events.rs` |
| Theme | `crates/mcp-agent-mail-server/src/tui_theme.rs` |
| Persistence | `crates/mcp-agent-mail-server/src/tui_persist.rs` |
| DB queries | `crates/mcp-agent-mail-db/src/queries.rs` |
| Tool metrics | `crates/mcp-agent-mail-tools/src/metrics.rs` |
