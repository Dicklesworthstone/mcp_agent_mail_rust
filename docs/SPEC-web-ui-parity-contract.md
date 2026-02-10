# Spec: Python <-> Rust Web UI Parity Contract

**Bead:** br-3vwi.13.1  
**Date:** 2026-02-10  
**Status:** Draft (contract + CI guard skeleton)

## Purpose

Define an externally observed parity contract between the legacy Python `/mail/*`
web UI and the Rust `/mail/*` web UI (and future static export surface).

This contract is intentionally:
- Human-readable: contributors can quickly see what must match, what is allowed
  to differ, and why.
- Machine-readable: CI can fail fast if rows are missing ownership or have an
  unknown conformance status.

## Parity Policy

- `must_match`: Required parity with legacy Python behavior.
- `approved_difference`: Allowed behavior drift (must include a rationale).

## Conformance Status

- `implemented`: Rust matches the legacy behavior (or the approved difference).
- `partial`: Rust exists but differs in important ways; needs follow-up.
- `gap`: Route/action missing in Rust.
- `waived`: Explicitly waived (only allowed with `approved_difference`).
- `pending_review`: Decision not finalized yet; must be resolved before closing `br-3vwi.13`.

## Evidence

Evidence pointers are intentionally concrete (file paths, tests, templates).
Line numbers are optional; prefer stable file+symbol references when possible.

## Machine-Readable Contract (CI-Enforced)

CI parses the JSON block below and fails if any row:
- has empty `owner_beads`, or
- uses `status: "unknown"` (not allowed), or
- uses an unknown `policy` / `status` enum value.

<!-- WEB_UI_PARITY_CONTRACT_JSON_START -->
```json
{
  "schema_version": 1,
  "rows": [
    {
      "id": "mail_api_locks_get",
      "category": "route_json",
      "method": "GET",
      "python_path": "/mail/api/locks",
      "rust_path": "/mail/api/locks",
      "policy": "must_match",
      "status": "implemented",
      "owner_beads": ["br-3vwi.13.2", "br-3vwi.13.9"],
      "evidence": [
        "legacy_python_mcp_agent_mail_code/mcp_agent_mail/src/mcp_agent_mail/http.py",
        "legacy_python_mcp_agent_mail_code/mcp_agent_mail/tests/test_mail_viewer_e2e.py",
        "crates/mcp-agent-mail-server/src/lib.rs (handle_special_routes)",
        "crates/mcp-agent-mail-server/src/mail_ui.rs"
      ],
      "notes": "Lock status JSON endpoint parity."
    },
    {
      "id": "mail_root_html_get",
      "category": "route_html",
      "method": "GET",
      "python_path": "/mail",
      "rust_path": "/mail",
      "policy": "must_match",
      "status": "gap",
      "owner_beads": ["br-3vwi.13.2"],
      "evidence": [
        "legacy_python_mcp_agent_mail_code/mcp_agent_mail/tests/test_mail_viewer_e2e.py (test_mail_unified_inbox_html)",
        "crates/mcp-agent-mail-server/src/mail_ui.rs (dispatch + render_index)"
      ],
      "notes": "Python: unified inbox HTML. Rust currently uses /mail as project index and unified inbox at /mail/unified-inbox."
    },
    {
      "id": "mail_unified_inbox_html_get",
      "category": "route_html",
      "method": "GET",
      "python_path": "/mail/unified-inbox",
      "rust_path": "/mail/unified-inbox",
      "policy": "must_match",
      "status": "implemented",
      "owner_beads": ["br-3vwi.13.2"],
      "evidence": [
        "legacy_python_mcp_agent_mail_code/mcp_agent_mail/tests/test_mail_viewer_e2e.py (test_mail_unified_inbox_alternate_route)",
        "crates/mcp-agent-mail-server/src/mail_ui.rs (render_unified_inbox)"
      ],
      "notes": "Unified inbox HTML (alternate route)."
    },
    {
      "id": "mail_unified_inbox_api_get",
      "category": "route_json",
      "method": "GET",
      "python_path": "/mail/api/unified-inbox",
      "rust_path": "/mail/api/unified-inbox",
      "policy": "must_match",
      "status": "partial",
      "owner_beads": ["br-3vwi.13.2", "br-3vwi.13.8"],
      "evidence": [
        "legacy_python_mcp_agent_mail_code/mcp_agent_mail/tests/test_mail_viewer_e2e.py (test_mail_unified_inbox_api)",
        "crates/mcp-agent-mail-server/src/mail_ui.rs (render_api_unified_inbox)"
      ],
      "notes": "Endpoint exists; JSON shape/field parity needs explicit contract + conformance tests."
    },
    {
      "id": "mail_projects_html_get",
      "category": "route_html",
      "method": "GET",
      "python_path": "/mail/projects",
      "rust_path": null,
      "policy": "must_match",
      "status": "gap",
      "owner_beads": ["br-3vwi.13.2"],
      "evidence": [
        "legacy_python_mcp_agent_mail_code/mcp_agent_mail/tests/test_mail_viewer_e2e.py (test_mail_projects_list)",
        "legacy_python_mcp_agent_mail_code/mcp_agent_mail/src/mcp_agent_mail/http.py",
        "crates/mcp-agent-mail-server/src/mail_ui.rs (dispatch)"
      ],
      "notes": "Legacy has an explicit projects list route."
    },
    {
      "id": "mail_project_html_get",
      "category": "route_html",
      "method": "GET",
      "python_path": "/mail/{project}",
      "rust_path": "/mail/{project}",
      "policy": "must_match",
      "status": "partial",
      "owner_beads": ["br-3vwi.13.2"],
      "evidence": [
        "legacy_python_mcp_agent_mail_code/mcp_agent_mail/tests/test_mail_viewer_e2e.py (test_mail_project_view_with_search)",
        "crates/mcp-agent-mail-server/src/mail_ui.rs (render_project)"
      ],
      "notes": "Route exists; legacy supports filtering via ?q= on the project view. Rust uses a dedicated /search route; decide and lock the contract."
    },
    {
      "id": "mail_projects_siblings_post",
      "category": "route_json",
      "method": "POST",
      "python_path": "/mail/api/projects/{project_id}/siblings/{other_id}",
      "rust_path": null,
      "policy": "must_match",
      "status": "gap",
      "owner_beads": ["br-3vwi.13.2"],
      "evidence": [
        "legacy_python_mcp_agent_mail_code/mcp_agent_mail/src/mcp_agent_mail/http.py",
        "legacy_python_mcp_agent_mail_code/mcp_agent_mail/src/mcp_agent_mail/app.py (sibling suggestion logic)",
        "crates/mcp-agent-mail-server/src/mail_ui.rs"
      ],
      "notes": "Legacy supports confirming/dismissing/resetting project sibling suggestions via this endpoint."
    },
    {
      "id": "mail_project_agents_api_get",
      "category": "route_json",
      "method": "GET",
      "python_path": "/mail/api/projects/{project}/agents",
      "rust_path": "/mail/api/projects/{project}/agents",
      "policy": "must_match",
      "status": "partial",
      "owner_beads": ["br-3vwi.13.2", "br-3vwi.13.8"],
      "evidence": [
        "legacy_python_mcp_agent_mail_code/mcp_agent_mail/src/mcp_agent_mail/http.py",
        "crates/mcp-agent-mail-server/src/mail_ui.rs (render_api_project_agents)"
      ],
      "notes": "Endpoint exists; JSON shape/ordering should be pinned by conformance tests."
    },
    {
      "id": "mail_project_inbox_html_get",
      "category": "route_html",
      "method": "GET",
      "python_path": "/mail/{project}/inbox/{agent}",
      "rust_path": "/mail/{project}/inbox/{agent}",
      "policy": "must_match",
      "status": "partial",
      "owner_beads": ["br-3vwi.13.2", "br-3vwi.13.8"],
      "evidence": [
        "legacy_python_mcp_agent_mail_code/mcp_agent_mail/tests/test_mail_viewer_e2e.py (test_mail_agent_inbox_view)",
        "crates/mcp-agent-mail-server/src/mail_ui.rs (render_inbox)"
      ],
      "notes": "HTML route exists; pagination/limit semantics should be locked via parity tests."
    },
    {
      "id": "mail_message_detail_html_get",
      "category": "route_html",
      "method": "GET",
      "python_path": "/mail/{project}/message/{mid}",
      "rust_path": "/mail/{project}/message/{mid}",
      "policy": "must_match",
      "status": "partial",
      "owner_beads": ["br-3vwi.13.2", "br-3vwi.13.3"],
      "evidence": [
        "legacy_python_mcp_agent_mail_code/mcp_agent_mail/tests/test_mail_viewer_e2e.py (test_mail_message_detail_view)",
        "crates/mcp-agent-mail-server/src/mail_ui.rs (render_message)",
        "crates/mcp-agent-mail-server/templates/mail_message.html"
      ],
      "notes": "Route exists; markdown rendering parity is tracked separately (br-3vwi.13.3)."
    },
    {
      "id": "mail_mark_read_post",
      "category": "action_post",
      "method": "POST",
      "python_path": "/mail/{project}/inbox/{agent}/mark-read",
      "rust_path": null,
      "policy": "must_match",
      "status": "gap",
      "owner_beads": ["br-3vwi.13.2"],
      "evidence": [
        "legacy_python_mcp_agent_mail_code/mcp_agent_mail/tests/test_mail_viewer_e2e.py (test_mail_mark_read)",
        "legacy_python_mcp_agent_mail_code/mcp_agent_mail/src/mcp_agent_mail/http.py"
      ],
      "notes": "Legacy supports marking a single message read from the web UI."
    },
    {
      "id": "mail_mark_all_read_post",
      "category": "action_post",
      "method": "POST",
      "python_path": "/mail/{project}/inbox/{agent}/mark-all-read",
      "rust_path": null,
      "policy": "must_match",
      "status": "gap",
      "owner_beads": ["br-3vwi.13.2"],
      "evidence": [
        "legacy_python_mcp_agent_mail_code/mcp_agent_mail/tests/test_mail_viewer_e2e.py (test_mail_mark_all_read)",
        "legacy_python_mcp_agent_mail_code/mcp_agent_mail/src/mcp_agent_mail/http.py"
      ],
      "notes": "Legacy supports bulk mark-all-read from the web UI."
    },
    {
      "id": "mail_thread_detail_html_get",
      "category": "route_html",
      "method": "GET",
      "python_path": "/mail/{project}/thread/{thread_id}",
      "rust_path": "/mail/{project}/thread/{thread_id}",
      "policy": "must_match",
      "status": "partial",
      "owner_beads": ["br-3vwi.13.2", "br-3vwi.13.3"],
      "evidence": [
        "legacy_python_mcp_agent_mail_code/mcp_agent_mail/tests/test_mail_viewer_e2e.py (test_mail_thread_view)",
        "crates/mcp-agent-mail-server/src/mail_ui.rs (render_thread)",
        "crates/mcp-agent-mail-server/templates/mail_thread.html"
      ],
      "notes": "Route exists; markdown rendering parity is tracked separately (br-3vwi.13.3)."
    },
    {
      "id": "mail_search_html_get",
      "category": "route_html",
      "method": "GET",
      "python_path": "/mail/{project}/search",
      "rust_path": "/mail/{project}/search",
      "policy": "must_match",
      "status": "partial",
      "owner_beads": ["br-3vwi.13.4", "br-3vwi.13.8"],
      "evidence": [
        "legacy_python_mcp_agent_mail_code/mcp_agent_mail/tests/test_mail_viewer_e2e.py (test_mail_search_page, test_mail_search_with_query)",
        "crates/mcp-agent-mail-server/src/mail_ui.rs (render_search)",
        "crates/mcp-agent-mail-server/templates/mail_search.html"
      ],
      "notes": "Route exists; advanced search UX semantics (facets/snippets/highlights) need explicit parity contract + tests."
    },
    {
      "id": "mail_file_reservations_html_get",
      "category": "route_html",
      "method": "GET",
      "python_path": "/mail/{project}/file_reservations",
      "rust_path": "/mail/{project}/file_reservations",
      "policy": "must_match",
      "status": "implemented",
      "owner_beads": ["br-3vwi.13.2"],
      "evidence": [
        "legacy_python_mcp_agent_mail_code/mcp_agent_mail/tests/test_mail_viewer_e2e.py (test_mail_file_reservations_view)",
        "crates/mcp-agent-mail-server/src/mail_ui.rs (render_file_reservations)"
      ],
      "notes": "File reservations HTML view."
    },
    {
      "id": "mail_attachments_html_get",
      "category": "route_html",
      "method": "GET",
      "python_path": "/mail/{project}/attachments",
      "rust_path": "/mail/{project}/attachments",
      "policy": "must_match",
      "status": "implemented",
      "owner_beads": ["br-3vwi.13.2"],
      "evidence": [
        "legacy_python_mcp_agent_mail_code/mcp_agent_mail/tests/test_mail_viewer_e2e.py (test_mail_attachments_view)",
        "crates/mcp-agent-mail-server/src/mail_ui.rs (render_attachments)"
      ],
      "notes": "Attachments browser HTML view."
    },
    {
      "id": "mail_overseer_compose_html_get",
      "category": "route_html",
      "method": "GET",
      "python_path": "/mail/{project}/overseer/compose",
      "rust_path": "/mail/{project}/overseer/compose",
      "policy": "must_match",
      "status": "implemented",
      "owner_beads": ["br-3vwi.13.2"],
      "evidence": [
        "legacy_python_mcp_agent_mail_code/mcp_agent_mail/tests/test_mail_overseer_compose",
        "crates/mcp-agent-mail-server/src/mail_ui.rs (render_overseer_compose)"
      ],
      "notes": "Human overseer compose form."
    },
    {
      "id": "mail_overseer_send_post",
      "category": "action_post",
      "method": "POST",
      "python_path": "/mail/{project}/overseer/send",
      "rust_path": null,
      "policy": "must_match",
      "status": "gap",
      "owner_beads": ["br-3vwi.13.2"],
      "evidence": [
        "legacy_python_mcp_agent_mail_code/mcp_agent_mail/tests/test_mail_overseer_send",
        "legacy_python_mcp_agent_mail_code/mcp_agent_mail/src/mcp_agent_mail/http.py"
      ],
      "notes": "Legacy supports web send action; Rust needs parity (or explicit approved difference)."
    },
    {
      "id": "mail_archive_guide_html_get",
      "category": "route_html",
      "method": "GET",
      "python_path": "/mail/archive/guide",
      "rust_path": "/mail/archive/guide",
      "policy": "must_match",
      "status": "implemented",
      "owner_beads": ["br-3vwi.13.2"],
      "evidence": [
        "legacy_python_mcp_agent_mail_code/mcp_agent_mail/src/mcp_agent_mail/http.py",
        "crates/mcp-agent-mail-server/src/mail_ui.rs (render_archive_guide)",
        "crates/mcp-agent-mail-server/templates/archive_guide.html"
      ],
      "notes": "Archive guide view."
    },
    {
      "id": "mail_archive_activity_html_get",
      "category": "route_html",
      "method": "GET",
      "python_path": "/mail/archive/activity",
      "rust_path": "/mail/archive/activity",
      "policy": "must_match",
      "status": "implemented",
      "owner_beads": ["br-3vwi.13.2"],
      "evidence": [
        "legacy_python_mcp_agent_mail_code/mcp_agent_mail/src/mcp_agent_mail/http.py",
        "crates/mcp-agent-mail-server/src/mail_ui.rs (render_archive_activity)"
      ],
      "notes": "Recent commits activity view."
    },
    {
      "id": "mail_archive_commit_html_get",
      "category": "route_html",
      "method": "GET",
      "python_path": "/mail/archive/commit/{sha}",
      "rust_path": "/mail/archive/commit/{sha}",
      "policy": "must_match",
      "status": "implemented",
      "owner_beads": ["br-3vwi.13.2"],
      "evidence": [
        "legacy_python_mcp_agent_mail_code/mcp_agent_mail/src/mcp_agent_mail/http.py",
        "crates/mcp-agent-mail-server/src/mail_ui.rs (render_archive_commit)"
      ],
      "notes": "Single commit detail view."
    },
    {
      "id": "mail_archive_timeline_html_get",
      "category": "route_html",
      "method": "GET",
      "python_path": "/mail/archive/timeline",
      "rust_path": "/mail/archive/timeline",
      "policy": "must_match",
      "status": "implemented",
      "owner_beads": ["br-3vwi.13.2"],
      "evidence": [
        "legacy_python_mcp_agent_mail_code/mcp_agent_mail/src/mcp_agent_mail/http.py",
        "crates/mcp-agent-mail-server/src/mail_ui.rs (render_archive_timeline)"
      ],
      "notes": "Archive timeline view."
    },
    {
      "id": "mail_archive_browser_html_get",
      "category": "route_html",
      "method": "GET",
      "python_path": "/mail/archive/browser",
      "rust_path": "/mail/archive/browser",
      "policy": "must_match",
      "status": "implemented",
      "owner_beads": ["br-3vwi.13.2"],
      "evidence": [
        "legacy_python_mcp_agent_mail_code/mcp_agent_mail/src/mcp_agent_mail/http.py",
        "crates/mcp-agent-mail-server/src/mail_ui.rs (render_archive_browser)"
      ],
      "notes": "Archive browser view."
    },
    {
      "id": "mail_archive_browser_file_get",
      "category": "route_json",
      "method": "GET",
      "python_path": "/mail/archive/browser/{project}/file",
      "rust_path": "/mail/archive/browser/{project}/file",
      "policy": "must_match",
      "status": "partial",
      "owner_beads": ["br-3vwi.13.2", "br-3vwi.13.8"],
      "evidence": [
        "legacy_python_mcp_agent_mail_code/mcp_agent_mail/src/mcp_agent_mail/http.py",
        "crates/mcp-agent-mail-server/src/mail_ui.rs (render_archive_browser_file)"
      ],
      "notes": "Both expose file content, but content-type/shape/size limits must be pinned."
    },
    {
      "id": "mail_archive_network_html_get",
      "category": "route_html",
      "method": "GET",
      "python_path": "/mail/archive/network",
      "rust_path": "/mail/archive/network",
      "policy": "must_match",
      "status": "implemented",
      "owner_beads": ["br-3vwi.13.2"],
      "evidence": [
        "legacy_python_mcp_agent_mail_code/mcp_agent_mail/src/mcp_agent_mail/http.py",
        "crates/mcp-agent-mail-server/src/mail_ui.rs (render_archive_network)"
      ],
      "notes": "Archive network view."
    },
    {
      "id": "mail_archive_time_travel_html_get",
      "category": "route_html",
      "method": "GET",
      "python_path": "/mail/archive/time-travel",
      "rust_path": "/mail/archive/time-travel",
      "policy": "must_match",
      "status": "implemented",
      "owner_beads": ["br-3vwi.13.2"],
      "evidence": [
        "legacy_python_mcp_agent_mail_code/mcp_agent_mail/src/mcp_agent_mail/http.py",
        "crates/mcp-agent-mail-server/src/mail_ui.rs (render_archive_time_travel)"
      ],
      "notes": "Time-travel view."
    },
    {
      "id": "mail_archive_time_travel_snapshot_get",
      "category": "route_json",
      "method": "GET",
      "python_path": "/mail/archive/time-travel/snapshot",
      "rust_path": "/mail/archive/time-travel/snapshot",
      "policy": "must_match",
      "status": "partial",
      "owner_beads": ["br-3vwi.13.2", "br-3vwi.13.8"],
      "evidence": [
        "legacy_python_mcp_agent_mail_code/mcp_agent_mail/src/mcp_agent_mail/http.py",
        "crates/mcp-agent-mail-server/src/mail_ui.rs (render_archive_time_travel_snapshot)"
      ],
      "notes": "Snapshot payload needs an explicit schema contract + conformance coverage."
    },
    {
      "id": "mail_security_xss_escaped",
      "category": "security",
      "method": "GET",
      "python_path": "/mail/{project}/search?q=<script>...",
      "rust_path": "/mail/{project}/search?q=<script>...",
      "policy": "must_match",
      "status": "partial",
      "owner_beads": ["br-3vwi.13.7", "br-3vwi.10.14"],
      "evidence": [
        "legacy_python_mcp_agent_mail_code/mcp_agent_mail/tests/test_mail_viewer_e2e.py (test_mail_xss_in_search_query, test_mail_xss_in_project_name)",
        "crates/mcp-agent-mail-server/tests/ui_markdown_templates.rs",
        "crates/mcp-agent-mail-server/src/templates.rs",
        "crates/mcp-agent-mail-server/templates/base.html"
      ],
      "notes": "HTML must escape query/path input; lock with dedicated Rust parity tests and regression boundaries."
    },
    {
      "id": "startup_transport_default_8765_mcp_path",
      "category": "startup_transport",
      "method": "N/A",
      "python_path": "http://127.0.0.1:8765/mcp/",
      "rust_path": "http://127.0.0.1:8765/mcp/",
      "policy": "must_match",
      "status": "implemented",
      "owner_beads": ["br-3vwi.13.9"],
      "evidence": [
        "docs/ADR-001-dual-mode-invariants.md",
        "docs/SPEC-interface-mode-switch.md",
        "crates/mcp-agent-mail/src/main.rs",
        "crates/mcp-agent-mail-server/src/lib.rs"
      ],
      "notes": "Legacy clients assume fixed default port/path for MCP HTTP transport."
    }
  ]
}
```
<!-- WEB_UI_PARITY_CONTRACT_JSON_END -->

## Next Steps (Follow-on Beads)

This contract is only the governance layer. Implementation lives in:
- `br-3vwi.13.2` (routes/actions parity)
- `br-3vwi.13.3` (markdown preview parity)
- `br-3vwi.13.4` (web search cockpit parity)
- `br-3vwi.13.7` + `br-3vwi.10.14` (permission/redaction + security E2E)
- `br-3vwi.13.8` (parity test matrix + forensic logging)
- `br-3vwi.13.9` (startup/transport compatibility lock)
