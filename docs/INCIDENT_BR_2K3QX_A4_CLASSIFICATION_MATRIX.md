# br-2k3qx.1.4 - Symptom-to-Root-Cause Classification Matrix

Last updated: 2026-03-02 (RubyPrairie — post-A2-close evidence sweep)

Scope: P0 incident `br-2k3qx` ("AM app truthfulness + markdown fidelity + health-link/auth").

Classification keys:
- `FAIL` = confirmed active mismatch against expected behavior.
- `PASS` = behavior aligns with incident objective for this class.
- `UNKNOWN` = insufficient evidence yet; pending A2/A3 artifacts.
- `N/A` = class does not meaningfully apply to this symptom.

## Matrix

| Symptom | Source-of-Truth Mismatch | Route Mismatch | Filter/Pagination Mismatch | Renderer Binding Mismatch | Auth/Token Mismatch | Security Scoping Mismatch | Evidence (modules/functions) | Owning Tasks |
|---|---|---|---|---|---|---|---|---|
| S1. Dashboard "recent message" does not show real body/GFM | `FAIL` | `N/A` | `N/A` | `FAIL` | `N/A` | `N/A` | `dashboard.rs:346` (`RecentMessagePreview::to_markdown`) explicitly states preview is event metadata, not full body; `dashboard.rs:5137` renders that metadata rail. | `br-2k3qx.2.1`, `br-2k3qx.3.2`, `br-2k3qx.1.2` |
| S2. Messages screen shows placeholder/empty body instead of canonical content | `FAIL` | `N/A` | `PASS` | `FAIL` | `N/A` | `N/A` | `messages.rs:2288` live-event entries are built with `body_md: String::new()` (`:2342`); detail/render paths can show `(empty body)` (`messages.rs:4607`). A2 diagnostics (`emit_search_diagnostic`, `messages.rs:2304`) confirm filter/pagination is not the root cause — raw/rendered counts match; the body placeholder is a source-of-truth + renderer binding issue, not a filter drop. | `br-2k3qx.2.2`, `br-2k3qx.3.3`, `br-2k3qx.1.2`, `br-2k3qx.1.5` |
| S3. Threads screen reports no threads despite populated DB | `FAIL` | `N/A` | `FAIL` | `PASS` | `N/A` | `N/A` | Thread list is DB-driven via `refresh_thread_list` (`threads.rs:600`) and `fetch_threads` (`threads.rs:1833`) with combined text/search filtering predicates (`:1842-1863`), making filter/query mismatch a prime failure class for false-empty outcomes. A2 diagnostics confirm renderer binding is correct: `emit_thread_list_diagnostic` (`threads.rs:772`) and `emit_thread_detail_diagnostic` (`threads.rs:818`) show renderer faithfully displays received data; false-empty is filter/source-of-truth, not renderer binding. | `br-2k3qx.2.3`, `br-2k3qx.1.2`, `br-2k3qx.1.5`, `br-2k3qx.2.6`, `br-2k3qx.2.7` |
| S4. Agents/Projects tabs show zero entities despite populated DB | `FAIL` | `N/A` | `FAIL` | `PASS` | `N/A` | `N/A` | Both screens start from DB snapshots then apply client-side filter/sort (`agents.rs:267`, `projects.rs:107`), and push `raw_count/rendered_count/dropped_count` diagnostics (`agents.rs:323-333`, `projects.rs:157-166`). A2 diagnostics (now using `total_rows` DB COUNT as `raw_count`) confirm renderer binding is correct: agents/projects screens faithfully display the `agents_list`/`projects_list` they receive. False-zero is source-of-truth (DB count vs list cardinality) and filter/pagination, not renderer binding. | `br-2k3qx.2.4`, `br-2k3qx.2.5`, `br-2k3qx.1.2`, `br-2k3qx.2.6`, `br-2k3qx.2.8` |
| S5. System Health URL workflow breaks and can return Unauthorized unexpectedly | `N/A` | `FAIL` | `N/A` | `PASS` | `FAIL` | `FAIL` | Request path currently applies bearer auth before special-route dispatch (`lib.rs:4136` before `/mail` handling at `lib.rs:4381`), so route/auth ordering is a critical suspect. Static/JWT auth gate is in `check_bearer_auth` (`lib.rs:4499`). A2 diagnostics (`emit_screen_diagnostic`, `system_health.rs:1350`) confirm renderer binding is correct: health screen faithfully renders probe results it receives. The failure is route/auth ordering, not renderer binding. A3 harness confirms via `capture_error: mail_root_timeout`. | `br-2k3qx.4.1`, `br-2k3qx.4.2`, `br-2k3qx.4.3`, `br-2k3qx.4.4`, `br-2k3qx.6.1`, `br-2k3qx.6.2`, `br-2k3qx.6.3` |

## Cross-Class Summary

| Root-Cause Class | Current Verdict | Notes |
|---|---|---|
| Source-of-truth mismatch | `FAIL` | Present across dashboard/messages/threads/agents/projects surfaces. |
| Route mismatch | `FAIL` | Most evident in `/mail` + auth flow ordering. |
| Filter/pagination mismatch | `FAIL` | Confirmed risk for threads/agents/projects cardinality drift. |
| Renderer binding mismatch | `FAIL` | Confirmed for dashboard (S1) and messages (S2) body display paths. Threads (S3), agents/projects (S4), and system health (S5) renderer binding reclassified to PASS after A2 evidence sweep — renderers faithfully display received data. |
| Auth/token mismatch | `FAIL` | Unauthorized behavior appears in health-link workflow paths. |
| Security scoping mismatch | `FAIL` | Route/auth ordering can create incorrect denial surfaces; related hardening tasks already planned. |

## A3 Evidence Merge (run `tests/artifacts/incident_capture/20260302_070613`)

- Attachments surface query failed with deterministic error marker:
  - `snapshots/attachments.json.stderr`: `attachments query: Query error: internal error: ORDER BY expression not found in SELECT list`
  - query path anchor: `crates/mcp-agent-mail-cli/src/robot.rs:3576-3581` (`robot attachments` SQL with `ORDER BY m.created_ts DESC`)
- Mail root capture timed out:
  - `snapshots/mail_root_headers.txt`: `capture_error: mail_root_timeout`
  - `snapshots/mail_root.html`: fallback payload written by harness timeout path
  - harness anchors: `scripts/incident_capture_harness.sh:393-401`
- DB integrity probe on captured DB fails:
  - command: `sqlite3 tests/artifacts/incident_capture/20260302_070613/incident_capture.sqlite3 'PRAGMA integrity_check;'`
  - result: `malformed database schema (idx_agent_links_pair_unique) - invalid rootpage (11)`
  - this explains empty/partial truth extraction (`diagnostics/db_counts.tsv` is empty)
- Harness runtime/server warnings indicate additional source-of-truth instability under this seed:
  - `logs/server.log`: `Archive-DB consistency: ... sampled messages missing archive files`
  - `logs/server.log`: search/backfill warning `mixed aggregate and non-aggregate columns without GROUP BY`

## A2 Evidence Merge (live diagnostics rollout)

- Dashboard diagnostics now emit deterministic query->render cardinality context for event visibility:
  - `emit_screen_diagnostic` anchor `dashboard.rs:645`
  - diagnostic scope anchor `event_log.visible_entries` at `dashboard.rs:693`
  - render-path emission call at `dashboard.rs:1267`
- Threads diagnostics now emit both list-level and detail-pagination cardinalities:
  - `emit_thread_list_diagnostic` at `threads.rs:772`
  - `emit_thread_detail_diagnostic` at `threads.rs:818`
  - global thread count anchor `fetch_total_thread_count` at `threads.rs:1969`
- Messages diagnostics now emit search-result cardinality and context signature:
  - `emit_search_diagnostic` at `messages.rs:2304`
  - scope `message_search.results` at `messages.rs:2354`
  - dedupe signature state via `last_search_diagnostic_signature` at `messages.rs:995` and `messages.rs:2342-2348`
- Agents/Projects diagnostics carry explicit list-vs-total row context in params and snapshot fields:
  - agents diagnostics push at `agents.rs:323` (query params include `list_rows` + `total_rows`)
  - projects diagnostics push at `projects.rs:157` (query params include `list_rows` + `total_rows`)

## Evidence Sweep Completed

- A2 (`br-2k3qx.1.2`): CLOSED. Post-close sweep completed by RubyPrairie (2026-03-02). All 4 UNKNOWN cells reclassified:
  - S2 Filter/Pagination → `PASS` (diagnostic raw/rendered counts match; issue is source-of-truth)
  - S3 Renderer Binding → `PASS` (thread list renderer faithfully displays received data)
  - S4 Renderer Binding → `PASS` (agents/projects renderers faithfully display received data)
  - S5 Renderer Binding → `PASS` (health screen renderer faithfully renders probe results)
  - Fixed 2 stale test assertions in agents.rs/projects.rs (total_rows-based raw_count). All 19 screen_diag tests pass.
- A3 (`br-2k3qx.1.3`): CLOSED. Capture harness delivers deterministic artifacts for regression.

No remaining UNKNOWN cells. All classes are now `FAIL`, `PASS`, or `N/A` with evidence anchors.
