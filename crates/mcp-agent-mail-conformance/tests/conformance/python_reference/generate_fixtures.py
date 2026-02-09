#!/usr/bin/env python3
"""Generate MCP Agent Mail conformance fixtures from legacy Python server.

We intentionally run the legacy Python FastMCP app **in-process** (no HTTP) and call
tool/resource functions directly. This avoids flaky network/transport issues and
keeps fixture generation fast.

Important: fixtures must be stable. We null-out volatile fields (timestamps, etc.)
and store the JSON Pointers we ignored under `case.normalize.ignore_json_pointers`,
so the Rust conformance runner can apply the same normalization.
"""

from __future__ import annotations

import asyncio
import json
import os
import subprocess
import sys
import tempfile
import uuid
from pathlib import Path
from typing import Any

_DEFAULT_FIXTURES = Path(__file__).resolve().parents[1] / "fixtures" / "python_reference.json"
# Allow callers (e.g. Rust CLI wrapper) to redirect fixture output to a temp file,
# so tests don't need to touch the tracked fixture under `tests/conformance/fixtures/`.
if "MCP_AGENT_MAIL_CONFORMANCE_FIXTURE_PATH" in os.environ:
    FIXTURES = Path(os.environ["MCP_AGENT_MAIL_CONFORMANCE_FIXTURE_PATH"]).expanduser()
else:
    FIXTURES = _DEFAULT_FIXTURES

LEGACY_VERSION = "legacy-python@0.3.0"
GENERATED_AT = "1970-01-01T00:00:00Z"  # keep diffs clean


def _escape_json_pointer_token(token: str) -> str:
    return token.replace("~", "~0").replace("/", "~1")


def _tokens_to_pointer(tokens: list[str]) -> str:
    if not tokens:
        return ""
    return "/" + "/".join(_escape_json_pointer_token(t) for t in tokens)


_VOLATILE_KEY_EXACT: frozenset[str] = frozenset(
    {
        # Common timestamps
        "created_at",
        "created_ts",
        "updated_at",
        "updated_ts",
        "inception_ts",
        "last_active_ts",
        "read_ts",
        "ack_ts",
        "expires_ts",
        "released_ts",
        "released_at",
        "acquired_ts",
        # Env-dependent
        "database_url",
        # Git-ish
        "hexsha",
        # Perf-ish
        "duration_ms",
        # Recent tool usage entries
        "timestamp",
        # Git diff excerpts may embed volatile timestamps.
        "excerpt",
        # Hook paths depend on temp repo roots.
        "hook",
        # Identity resource fields (paths are machine-specific).
        "git_toplevel",
        "git_common_dir",
    }
)


def _is_volatile_key(key: str) -> bool:
    if key in _VOLATILE_KEY_EXACT:
        return True
    if key.endswith("_ts") or key.endswith("_at"):
        return True
    # Paths are unstable across machines (temp dirs, checkout roots).
    if key.endswith("_path") or key.endswith("_paths"):
        return True
    return False


def _null_volatile_fields_inplace(value: Any) -> list[str]:
    """Null volatile keys in-place and return JSON Pointer paths that were nulled."""
    ignored: list[str] = []

    def walk(v: Any, tokens: list[str]) -> None:
        if isinstance(v, dict):
            for k in list(v.keys()):
                child_tokens = [*tokens, str(k)]
                if _is_volatile_key(str(k)):
                    v[k] = None
                    ignored.append(_tokens_to_pointer(child_tokens))
                    continue
                walk(v[k], child_tokens)
        elif isinstance(v, list):
            for i, item in enumerate(v):
                walk(item, [*tokens, str(i)])

    walk(value, [])
    # Deterministic ordering
    return sorted(set(ignored))


def _mk_run_dir() -> Path:
    """Create a writable per-run directory for storage + DB.

    IMPORTANT: Do not write scratch outputs into the committed fixture tree under this repo.
    The conformance fixtures are tracked and must remain immutable unless explicitly updated.

    Override (for debugging only):
    - Set `MCP_AGENT_MAIL_CONFORMANCE_SCRATCH_ROOT` to force the run dir base.
    - If you intentionally want scratch outputs *inside the repo*, also set
      `MCP_AGENT_MAIL_CONFORMANCE_ALLOW_REPO_SCRATCH=1`.
    """

    scratch_root_env = os.environ.get("MCP_AGENT_MAIL_CONFORMANCE_SCRATCH_ROOT", "").strip()
    allow_repo_scratch = os.environ.get("MCP_AGENT_MAIL_CONFORMANCE_ALLOW_REPO_SCRATCH", "").strip().lower() in {
        "1",
        "true",
        "yes",
        "on",
    }

    python_reference_dir = Path(__file__).resolve().parent

    if scratch_root_env:
        scratch_root = Path(scratch_root_env).expanduser().resolve()
        try:
            scratch_root.relative_to(python_reference_dir)
        except Exception:
            pass
        else:
            if not allow_repo_scratch:
                raise RuntimeError(
                    "Refusing to write conformance scratch outputs under the committed fixture tree. "
                    "Set MCP_AGENT_MAIL_CONFORMANCE_ALLOW_REPO_SCRATCH=1 to override."
                )

        scratch_root.mkdir(parents=True, exist_ok=True)
        run_dir = scratch_root / f"run_{uuid.uuid4().hex}"
        run_dir.mkdir(parents=True, exist_ok=True)
        return run_dir

    # Default: OS temp dir (never inside the repo).
    return Path(tempfile.mkdtemp(prefix="am-conformance-python_reference-run_"))


def _init_git_repo(path: Path) -> Path:
    path.mkdir(parents=True, exist_ok=True)
    subprocess.run(["git", "-C", str(path), "init"], check=True, capture_output=True, text=True)
    return path


def _set_legacy_env(run_dir: Path) -> None:
    # Make legacy server deterministic and quiet.
    os.environ.setdefault("APP_ENVIRONMENT", "development")
    os.environ["WORKTREES_ENABLED"] = "1"
    os.environ.setdefault("HTTP_HOST", "127.0.0.1")
    os.environ.setdefault("HTTP_PORT", "8765")
    os.environ.setdefault("HTTP_PATH", "/api/")

    # Deterministic LLM paths for llm_mode fixtures.
    os.environ.setdefault("LLM_ENABLED", "true")
    os.environ.setdefault("MCP_AGENT_MAIL_LLM_STUB", "1")

    # Disable rich tool call panels (we generate fixtures, not interactive UX).
    os.environ["TOOLS_LOG_ENABLED"] = "false"
    os.environ["LOG_RICH_ENABLED"] = "false"

    # Hermetic storage + DB inside the run dir.
    os.environ["STORAGE_ROOT"] = str((run_dir / "archive").resolve())
    os.environ["DATABASE_URL"] = f"sqlite+aiosqlite:///{(run_dir / 'db.sqlite3').resolve()}"


def _patch_legacy_noise() -> None:
    # The legacy server uses a Rich panel as *git commit message* via `_render_commit_panel`.
    # That helper currently renders via a Console bound to stderr, which prints the panel
    # as a side effect. For fixture generation we suppress it.
    try:
        import mcp_agent_mail.app as app_mod

        app_mod._render_commit_panel = lambda *args, **kwargs: None  # type: ignore[assignment]

        # Deterministic LLM completions (no network) for llm_mode conformance fixtures.
        import mcp_agent_mail.llm as llm_mod

        original_complete_system_user = llm_mod.complete_system_user

        async def stub_complete_system_user(
            system: str,
            user: str,
            *,
            model: str | None = None,
            temperature: float | None = None,
            max_tokens: int | None = None,
        ) -> llm_mod.LlmOutput:
            enabled = os.environ.get("MCP_AGENT_MAIL_LLM_STUB", "").strip().lower() in {
                "1",
                "true",
                "yes",
                "on",
            }
            if not enabled:
                return await original_complete_system_user(
                    system,
                    user,
                    model=model,
                    temperature=temperature,
                    max_tokens=max_tokens,
                )

            sys_lower = (system or "").lower()
            if "digest across threads" in sys_lower or user.startswith("Digest these threads:") or "# Thread" in user:
                content = (
                    "Ok, here's the digest:\n"
                    "{\n"
                    "  \"threads\": [\n"
                    "    {\n"
                    "      \"thread_id\": \"T-1\",\n"
                    "      \"key_points\": [\"API v2 schema finalized\"],\n"
                    "      \"actions\": [\"Update OpenAPI spec\"]\n"
                    "    },\n"
                    "    {\n"
                    "      \"thread_id\": \"T-2\",\n"
                    "      \"key_points\": [\"Migration to new DB\"],\n"
                    "      \"actions\": [\"Run migration script\"]\n"
                    "    }\n"
                    "  ],\n"
                    "  \"aggregate\": {\n"
                    "    \"top_mentions\": [\"Alice\", \"Bob\"],\n"
                    "    \"key_points\": [\"API schema and DB migration are the two main workstreams\"],\n"
                    "    \"action_items\": [\"Update OpenAPI spec\", \"Run migration script\"]\n"
                    "  }\n"
                    "}\n"
                    "Done."
                )
                return llm_mod.LlmOutput(
                    content=content,
                    model=str(model or "stub"),
                    provider="stub",
                    estimated_cost_usd=0.0,
                )

            content = (
                "Here is the summary:\n"
                "```json\n"
                "{\n"
                "  \"participants\": [\"BlueLake\", \"GreenCastle\"],\n"
                "  \"key_points\": [\n"
                "    \"API migration planned for next sprint\",\n"
                "    \"Staging deployment needed before review\"\n"
                "  ],\n"
                "  \"action_items\": [\"Deploy to staging\", \"Update API docs\"],\n"
                "  \"mentions\": [{\"name\": \"Carol\", \"count\": 2}],\n"
                "  \"code_references\": [\"api/v2/users\"],\n"
                "  \"total_messages\": 2,\n"
                "  \"open_actions\": 1,\n"
                "  \"done_actions\": 0\n"
                "}\n"
                "```\n"
            )
            return llm_mod.LlmOutput(
                content=content,
                model=str(model or "stub"),
                provider="stub",
                estimated_cost_usd=0.0,
            )

        llm_mod.complete_system_user = stub_complete_system_user  # type: ignore[assignment]
        # app.py imported complete_system_user into its module global; patch that too.
        app_mod.complete_system_user = stub_complete_system_user  # type: ignore[assignment]
    except Exception:
        return


def _make_quiet_ctx(mcp: Any) -> Any:
    # Create an actual FastMCP Context instance so internal macro tool calls using
    # `FunctionTool.run(...)` pass type validation (expects fastmcp Context).
    from fastmcp.server.context import Context as FastMCPContext

    ctx = FastMCPContext(mcp)

    async def _noop(*_args: Any, **_kwargs: Any) -> None:
        return None

    # Avoid touching request_ctx-backed internals (ctx.info -> ctx.log -> request_ctx).
    ctx.debug = _noop  # type: ignore[assignment]
    ctx.info = _noop  # type: ignore[assignment]
    ctx.warning = _noop  # type: ignore[assignment]
    ctx.error = _noop  # type: ignore[assignment]
    ctx.log = _noop  # type: ignore[assignment]

    return ctx


async def _call_tool(mcp: Any, ctx: Any, name: str, args: dict[str, Any]) -> Any:
    tool = await mcp.get_tool(name)
    # Always request JSON to keep fixture values structured.
    result = await tool.fn(ctx, **args, format="json")

    # Some tools return FastMCP ToolResult wrappers; unwrap to structured JSON.
    try:
        from fastmcp.tools.tool import ToolResult

        if isinstance(result, ToolResult):
            if result.structured_content is not None:
                return result.structured_content
            # Fall back to best-effort JSON decoding from text content.
            if len(result.content) == 1:
                block = result.content[0]
                text = getattr(block, "text", None)
                if isinstance(text, str):
                    try:
                        return json.loads(text)
                    except Exception:
                        return text
            return {"content": [getattr(b, "model_dump", lambda: str(b))() for b in result.content]}
    except Exception:
        pass

    return result


async def _read_resource_json(mcp: Any, uri: str) -> Any:
    read_uri = uri
    if "?" in uri:
        # Avoid duplicating format=json when callers already include it.
        _base, query = uri.split("?", 1)
        has_format = any(part.split("=", 1)[0] == "format" for part in query.split("&") if part)
        if not has_format:
            read_uri = f"{uri}&format=json"
    else:
        read_uri = f"{uri}?format=json"

    contents = await mcp._read_resource_mcp(read_uri)
    if not contents:
        raise RuntimeError(f"resource returned no contents: {uri}")
    item = contents[0]
    text = getattr(item, "content", None)
    if not isinstance(text, str):
        raise RuntimeError(f"resource returned non-text content: {uri}")
    return json.loads(text)


async def _generate() -> dict[str, Any]:
    run_dir = _mk_run_dir()
    _set_legacy_env(run_dir)
    _patch_legacy_noise()

    # Imports must happen after env + patching so cached settings and closure globals see it.
    from mcp_agent_mail.app import build_mcp_server

    mcp = build_mcp_server()
    ctx = _make_quiet_ctx(mcp)

    tools: dict[str, Any] = {}
    resources: dict[str, Any] = {}

    async with mcp._lifespan_manager():
        async def record_tool(tool_name: str, case_name: str, tool_args: dict[str, Any]) -> Any:
            out = await _call_tool(mcp, ctx, tool_name, tool_args)
            ignored = _null_volatile_fields_inplace(out)
            case: dict[str, Any] = {
                "name": case_name,
                "input": tool_args,
                "expect": {"ok": out},
            }
            if ignored:
                case["normalize"] = {"ignore_json_pointers": ignored}

            tool_entry = tools.setdefault(tool_name, {"cases": []})
            tool_entry["cases"].append(case)
            return out

        async def record_tool_error(
            tool_name: str,
            case_name: str,
            tool_args: dict[str, Any],
            message_contains: str,
        ) -> None:
            try:
                await _call_tool(mcp, ctx, tool_name, tool_args)
            except Exception as exc:
                msg = str(exc)
                if message_contains not in msg:
                    raise RuntimeError(
                        f"expected error containing {message_contains!r}, got {msg!r}"
                    ) from exc
                case = {
                    "name": case_name,
                    "input": tool_args,
                    "expect": {"err": {"message_contains": message_contains}},
                }
                tool_entry = tools.setdefault(tool_name, {"cases": []})
                tool_entry["cases"].append(case)
                return
            raise RuntimeError(f"expected error for tool {tool_name} case {case_name}")

        # --- Tool scenario (ordered) ---------------------------------------------------------
        await record_tool("health_check", "default", {})

        ensure_project_out = await record_tool(
            "ensure_project",
            "abs_path_backend",
            {"human_key": "/abs/path/backend"},
        )
        await record_tool_error(
            "ensure_project",
            "relative_path_error",
            {"human_key": "./backend"},
            "absolute directory path",
        )
        project_slug = ensure_project_out["slug"]
        repo_root = Path(
            os.environ.get("AM_FIXTURE_REPO_ROOT", "/tmp/agent-mail-fixtures")
        ).resolve()
        repo_install = _init_git_repo(repo_root / "repo_install")
        repo_uninstall = _init_git_repo(repo_root / "repo_uninstall")

        # --- Product Bus / Build Slots (worktrees-enabled) ---------------------------------
        ensure_product_out = await record_tool(
            "ensure_product",
            "product_widget_bus",
            {
                # Deterministic UID: legacy uses product_key when it matches hex(8..64).
                "product_key": "0123456789abcdef0123",
                "name": "WidgetBus",
            },
        )
        product_key = ensure_product_out["product_uid"]

        await record_tool(
            "products_link",
            "link_product_to_backend",
            {
                "product_key": product_key,
                "project_key": project_slug,
            },
        )

        await record_tool(
            "acquire_build_slot",
            "acquire_build_slot_default",
            {
                "project_key": project_slug,
                "agent_name": "BlueLake",
                "slot": "build",
                "ttl_seconds": 3600,
                "exclusive": True,
            },
        )
        await record_tool(
            "renew_build_slot",
            "renew_build_slot_default",
            {
                "project_key": project_slug,
                "agent_name": "BlueLake",
                "slot": "build",
                "extend_seconds": 600,
            },
        )
        await record_tool(
            "release_build_slot",
            "release_build_slot_default",
            {
                "project_key": project_slug,
                "agent_name": "BlueLake",
                "slot": "build",
            },
        )

        await record_tool(
            "register_agent",
            "blue_lake",
            {
                "project_key": project_slug,
                "program": "codex-cli",
                "model": "gpt-5",
                "name": "BlueLake",
                "task_description": "sender",
            },
        )
        await record_tool(
            "register_agent",
            "green_castle",
            {
                "project_key": project_slug,
                "program": "codex-cli",
                "model": "gpt-5",
                "name": "GreenCastle",
                "task_description": "recipient",
            },
        )

        await record_tool(
            "create_agent_identity",
            "create_orange_fox",
            {
                "project_key": project_slug,
                "program": "codex-cli",
                "model": "gpt-5",
                "name_hint": "OrangeFox",
                "task_description": "fresh identity",
            },
        )

        await record_tool(
            "request_contact",
            "bl_to_gc",
            {
                "project_key": project_slug,
                "from_agent": "BlueLake",
                "to_agent": "GreenCastle",
                "ttl_seconds": 86400,
            },
        )
        await record_tool(
            "respond_contact",
            "gc_approves_bl",
            {
                "project_key": project_slug,
                "from_agent": "BlueLake",
                "to_agent": "GreenCastle",
                "accept": True,
                "ttl_seconds": 86400,
            },
        )

        await record_tool(
            "list_contacts",
            "list_contacts_bluelake",
            {
                "project_key": project_slug,
                "agent_name": "BlueLake",
            },
        )
        await record_tool(
            "list_contacts",
            "list_contacts_greencastle",
            {
                "project_key": project_slug,
                "agent_name": "GreenCastle",
            },
        )

        await record_tool(
            "set_contact_policy",
            "set_contact_policy_gc_contacts_only",
            {
                "project_key": project_slug,
                "agent_name": "GreenCastle",
                "policy": "contacts_only",
            },
        )

        send_out = await record_tool(
            "send_message",
            "urgent_ack_required",
            {
                "project_key": project_slug,
                "sender_name": "BlueLake",
                "to": ["GreenCastle"],
                "subject": "Hello",
                "body_md": "Test",
                "importance": "urgent",
                "ack_required": True,
            },
        )
        message_id = send_out["deliveries"][0]["payload"]["id"]

        await record_tool_error(
            "send_message",
            "nonexistent_sender_error",
            {
                "project_key": project_slug,
                "sender_name": "NonExistentAgent",
                "to": ["GreenCastle"],
                "subject": "Should fail",
                "body_md": "This should fail because sender doesn't exist.",
            },
            "not found",
        )

        # --- LLM mode threads (created before any replies) --------------------------------
        # Conformance runner executes tool fixtures grouped by tool name. To keep message_id
        # references stable across tools, we create all send_message cases before reply_message.
        t1_out = await record_tool(
            "send_message",
            "llm_thread_t1_root",
            {
                "project_key": project_slug,
                "sender_name": "BlueLake",
                "to": ["GreenCastle"],
                "subject": "LLM Thread T-1",
                "body_md": "- TODO: deploy to staging\n- discussed API changes\n",
                "thread_id": "T-1",
            },
        )
        t1_message_id = t1_out["deliveries"][0]["payload"]["id"]

        t2_out = await record_tool(
            "send_message",
            "llm_thread_t2_root",
            {
                "project_key": project_slug,
                "sender_name": "BlueLake",
                "to": ["GreenCastle"],
                "subject": "LLM Thread T-2",
                "body_md": "- NEXT: migrate DB\n- ACTION: run script\n",
                "thread_id": "T-2",
            },
        )
        t2_message_id = t2_out["deliveries"][0]["payload"]["id"]

        await record_tool(
            "reply_message",
            "reply_in_thread",
            {
                "project_key": project_slug,
                "message_id": message_id,
                "sender_name": "GreenCastle",
                "body_md": "Reply",
            },
        )
        await record_tool(
            "reply_message",
            "llm_thread_t1_reply",
            {
                "project_key": project_slug,
                "message_id": t1_message_id,
                "sender_name": "GreenCastle",
                "body_md": "- ACK: will deploy\n- [ ] TODO: update docs\n`api/v2/users`\n@Carol\n",
            },
        )
        await record_tool(
            "reply_message",
            "llm_thread_t2_reply",
            {
                "project_key": project_slug,
                "message_id": t2_message_id,
                "sender_name": "GreenCastle",
                "body_md": "- BLOCKED: waiting on infra\n",
            },
        )

        await record_tool(
            "fetch_inbox",
            "gc_inbox_with_bodies",
            {
                "project_key": project_slug,
                "agent_name": "GreenCastle",
                "include_bodies": True,
                "limit": 10,
            },
        )
        await record_tool(
            "mark_message_read",
            "mark_contact_request_read",
            {
                "project_key": project_slug,
                "agent_name": "GreenCastle",
                # request_contact sends an intro mail first; this should be id=1 in a fresh DB.
                "message_id": 1,
            },
        )
        await record_tool(
            "acknowledge_message",
            "ack_contact_request",
            {
                "project_key": project_slug,
                "agent_name": "GreenCastle",
                "message_id": 1,
            },
        )

        await record_tool(
            "search_messages",
            "search_hello",
            {
                "project_key": project_slug,
                "query": "Hello",
                "limit": 10,
            },
        )
        await record_tool(
            "search_messages",
            "empty_query_returns_empty",
            {
                "project_key": project_slug,
                "query": "",
                "limit": 10,
            },
        )
        await record_tool_error(
            "search_messages",
            "project_not_found_error",
            {
                "project_key": "ZzzNonExistentProject",
                "query": "Hello",
                "limit": 10,
            },
            "not found",
        )
        await record_tool(
            "summarize_thread",
            "summarize_thread_root",
            {
                "project_key": project_slug,
                "thread_id": str(message_id),
                "include_examples": True,
                "llm_mode": False,
            },
        )
        await record_tool(
            "summarize_thread",
            "empty_thread_id_returns_empty_digest",
            {
                "project_key": project_slug,
                "thread_id": "",
                "include_examples": True,
                "llm_mode": False,
            },
        )
        await record_tool_error(
            "summarize_thread",
            "project_not_found_error",
            {
                "project_key": "ZzzNonExistentProject",
                "thread_id": str(message_id),
                "include_examples": True,
                "llm_mode": False,
            },
            "not found",
        )

        await record_tool(
            "search_messages_product",
            "product_search_hello",
            {
                "product_key": product_key,
                "query": "Hello",
                "limit": 10,
            },
        )
        await record_tool(
            "fetch_inbox_product",
            "product_inbox_green_castle_with_bodies",
            {
                "product_key": product_key,
                "agent_name": "GreenCastle",
                "include_bodies": True,
                "limit": 10,
            },
        )
        await record_tool(
            "summarize_thread_product",
            "product_summarize_thread_root",
            {
                "product_key": product_key,
                "thread_id": str(message_id),
                "include_examples": True,
                "llm_mode": False,
            },
        )

        # --- LLM mode conformance fixtures -----------------------------------------------
        await record_tool(
            "summarize_thread",
            "summarize_thread_t1_llm",
            {
                "project_key": project_slug,
                "thread_id": "T-1",
                "include_examples": True,
                "llm_mode": True,
            },
        )
        await record_tool(
            "summarize_thread_product",
            "product_summarize_thread_t1_llm",
            {
                "product_key": product_key,
                "thread_id": "T-1",
                "include_examples": True,
                "llm_mode": True,
            },
        )

        await record_tool(
            "summarize_thread",
            "summarize_thread_multi_llm",
            {
                "project_key": project_slug,
                "thread_id": "T-1,T-2",
                "llm_mode": True,
            },
        )

        await record_tool_error(
            "file_reservation_paths",
            "empty_paths_error",
            {
                "project_key": project_slug,
                "agent_name": "BlueLake",
                "paths": [],
            },
            "paths list cannot be empty",
        )
        await record_tool(
            "file_reservation_paths",
            "reserve_src_glob",
            {
                "project_key": project_slug,
                "agent_name": "BlueLake",
                "paths": ["src/**"],
                "ttl_seconds": 3600,
                "exclusive": True,
                "reason": "br-123",
            },
        )
        await record_tool(
            "renew_file_reservations",
            "renew_by_agent_all",
            {
                "project_key": project_slug,
                "agent_name": "BlueLake",
                "extend_seconds": 600,
            },
        )
        await record_tool(
            "renew_file_reservations",
            "extend_seconds_clamped_to_60",
            {
                "project_key": project_slug,
                "agent_name": "BlueLake",
                "extend_seconds": 5,
            },
        )
        await record_tool_error(
            "renew_file_reservations",
            "agent_not_found_error",
            {
                "project_key": project_slug,
                "agent_name": "ZzzNonExistentAgent",
                "extend_seconds": 600,
            },
            "not found",
        )
        await record_tool(
            "release_file_reservations",
            "release_by_agent_all",
            {
                "project_key": project_slug,
                "agent_name": "BlueLake",
            },
        )
        await record_tool_error(
            "force_release_file_reservation",
            "force_release_missing",
            {
                "project_key": project_slug,
                "agent_name": "BlueLake",
                "file_reservation_id": 9999,
            },
            "not found",
        )
        await record_tool(
            "install_precommit_guard",
            "install_precommit_guard_default",
            {
                "project_key": project_slug,
                "code_repo_path": str(repo_install),
            },
        )
        await record_tool(
            "uninstall_precommit_guard",
            "uninstall_precommit_guard_default",
            {
                "code_repo_path": str(repo_uninstall),
            },
        )

        await record_tool(
            "whois",
            "whois_bluelake",
            {
                "project_key": project_slug,
                "agent_name": "BlueLake",
                "include_recent_commits": False,
            },
        )
        await record_tool_error(
            "whois",
            "whois_nonexistent_error",
            {
                "project_key": project_slug,
                "agent_name": "ZzzNonExistentAgent",
            },
            "not found",
        )

        await record_tool(
            "macro_start_session",
            "macro_start_session_basic",
            {
                "human_key": "/abs/path/macro-session",
                "program": "codex-cli",
                "model": "gpt-5",
                "agent_name": "PurpleBear",
                "inbox_limit": 5,
            },
        )
        await record_tool(
            "macro_prepare_thread",
            "macro_prepare_thread_no_llm",
            {
                "project_key": project_slug,
                "thread_id": str(message_id),
                "program": "codex-cli",
                "model": "gpt-5",
                "agent_name": "BlueLake",
                "include_examples": True,
                "llm_mode": False,
            },
        )
        await record_tool(
            "macro_prepare_thread",
            "macro_prepare_thread_llm",
            {
                "project_key": project_slug,
                "thread_id": "T-1",
                "program": "codex-cli",
                "model": "gpt-5",
                "agent_name": "BlueLake",
                "include_examples": True,
                "llm_mode": True,
            },
        )
        await record_tool(
            "macro_file_reservation_cycle",
            "macro_reserve_and_release",
            {
                "project_key": project_slug,
                "agent_name": "BlueLake",
                "paths": ["src/**"],
                "ttl_seconds": 3600,
                "exclusive": True,
                "reason": "br-123",
                "auto_release": True,
            },
        )
        await record_tool(
            "macro_contact_handshake",
            "macro_handshake_auto_accept",
            {
                "project_key": project_slug,
                "requester": "BlueLake",
                "target": "GreenCastle",
                "auto_accept": True,
                "ttl_seconds": 86400,
            },
        )

        # --- Resource reads (post-scenario snapshot) ----------------------------------------
        resource_uris: list[tuple[str, str]] = []

        resource_uris.append(("resource://config/environment", "default"))
        # Static resources with query params must be routed via FastMCP URI templates.
        # Include explicit format cases so the Rust router must match `...?format=json`.
        resource_uris.append(("resource://config/environment?format=json", "default_format_json"))
        resource_uris.append(("resource://projects", "all_projects"))
        resource_uris.append(("resource://projects?format=json", "all_projects_format_json"))
        resource_uris.append((f"resource://project/{project_slug}", "project_detail"))
        resource_uris.append((f"resource://identity/{project_slug}", "identity_project"))
        resource_uris.append((f"resource://agents/{project_slug}", "agents_list"))
        resource_uris.append((f"resource://product/{product_key}", "product_detail"))
        resource_uris.append((f"resource://inbox/GreenCastle?project={project_slug}&include_bodies=true&limit=10", "inbox_resource"))
        resource_uris.append((f"resource://message/{message_id}?project={project_slug}", "message_detail"))
        resource_uris.append((f"resource://thread/{message_id}?project={project_slug}&include_bodies=true", "thread_detail"))
        resource_uris.append((f"resource://file_reservations/{project_slug}?active_only=false", "file_reservations_all"))
        resource_uris.append((f"resource://tooling/directory", "tooling_directory"))
        resource_uris.append((f"resource://tooling/directory?format=json", "tooling_directory_format_json"))
        resource_uris.append((f"resource://tooling/schemas", "tooling_schemas"))
        resource_uris.append((f"resource://tooling/schemas?format=json", "tooling_schemas_format_json"))
        resource_uris.append((f"resource://tooling/metrics", "tooling_metrics"))
        resource_uris.append((f"resource://tooling/metrics?format=json", "tooling_metrics_format_json"))
        resource_uris.append((f"resource://tooling/locks", "tooling_locks"))
        resource_uris.append((f"resource://tooling/locks?format=json", "tooling_locks_format_json"))
        resource_uris.append((f"resource://tooling/capabilities/BlueLake?project={project_slug}", "tooling_capabilities"))
        resource_uris.append((f"resource://tooling/recent/60?agent=BlueLake&project={project_slug}", "tooling_recent"))
        resource_uris.append((f"resource://views/urgent-unread/GreenCastle?project={project_slug}&limit=10", "urgent_unread"))
        resource_uris.append((f"resource://views/ack-required/GreenCastle?project={project_slug}&limit=10", "ack_required"))
        resource_uris.append((f"resource://views/acks-stale/GreenCastle?project={project_slug}&ttl_seconds=60&limit=10", "acks_stale"))
        resource_uris.append((f"resource://views/ack-overdue/GreenCastle?project={project_slug}&ttl_minutes=1&limit=10", "ack_overdue"))
        resource_uris.append((f"resource://mailbox/GreenCastle?project={project_slug}&limit=10", "mailbox"))
        resource_uris.append((f"resource://mailbox-with-commits/GreenCastle?project={project_slug}&limit=10", "mailbox_with_commits"))
        resource_uris.append((f"resource://outbox/BlueLake?project={project_slug}&limit=10&include_bodies=true", "outbox"))

        for uri, case_name in resource_uris:
            out = await _read_resource_json(mcp, uri)
            ignored = _null_volatile_fields_inplace(out)
            case = {
                "name": case_name,
                "input": {},  # query params are encoded into the URI for now
                "expect": {"ok": out},
            }
            if ignored:
                case["normalize"] = {"ignore_json_pointers": ignored}

            res_entry = resources.setdefault(uri, {"cases": []})
            res_entry["cases"].append(case)

    return {
        "version": LEGACY_VERSION,
        "generated_at": GENERATED_AT,
        "tools": tools,
        "resources": resources,
    }


def main() -> None:
    try:
        data = asyncio.run(_generate())
    except KeyboardInterrupt:
        raise
    except Exception as exc:
        print(f"[fixture-gen] ERROR: {exc}", file=sys.stderr)
        raise

    FIXTURES.parent.mkdir(parents=True, exist_ok=True)
    FIXTURES.write_text(json.dumps(data, indent=2) + "\n", encoding="utf-8")
    print(f"Wrote fixtures to {FIXTURES}")


if __name__ == "__main__":
    main()
