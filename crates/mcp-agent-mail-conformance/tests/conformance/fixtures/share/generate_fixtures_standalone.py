#!/usr/bin/env python3
"""Generate share/export conformance fixtures without native sqlite bindings."""

from __future__ import annotations

import hashlib
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
from dataclasses import asdict, dataclass
from pathlib import Path


SCRIPT_DIR = Path(__file__).resolve().parent
OUTPUT_DIR = SCRIPT_DIR
AM_BIN = os.environ.get("AM_CLI_BIN", "am")
TS = "2026-01-15T12:00:00+00:00"

SECRET_PATTERNS = (
    re.compile(r"ghp_[A-Za-z0-9]{36,}", re.IGNORECASE),
    re.compile(r"github_pat_[A-Za-z0-9_]{20,}", re.IGNORECASE),
    re.compile(r"xox[baprs]-[A-Za-z0-9-]{10,}", re.IGNORECASE),
    re.compile(r"sk-[A-Za-z0-9]{20,}", re.IGNORECASE),
    re.compile(r"(?i)bearer\s+[A-Za-z0-9_\-\.]{16,}"),
    re.compile(r"eyJ[0-9A-Za-z_-]+\.[0-9A-Za-z_-]+\.[0-9A-Za-z_-]+"),
)

ATTACHMENT_REDACT_KEYS = frozenset(
    {"download_url", "headers", "authorization", "signed_url", "bearer_token"}
)

SCRUB_PRESETS = {
    "standard": {
        "redact_body": False,
        "body_placeholder": None,
        "drop_attachments": False,
        "scrub_secrets": True,
        "clear_ack_state": True,
        "clear_recipients": True,
        "clear_file_reservations": True,
        "clear_agent_links": True,
    },
    "strict": {
        "redact_body": True,
        "body_placeholder": "[Message body redacted]",
        "drop_attachments": False,
        "scrub_secrets": True,
        "clear_ack_state": True,
        "clear_recipients": True,
        "clear_file_reservations": True,
        "clear_agent_links": True,
    },
    "archive": {
        "redact_body": False,
        "body_placeholder": None,
        "drop_attachments": True,
        "scrub_secrets": True,
        "clear_ack_state": False,
        "clear_recipients": False,
        "clear_file_reservations": True,
        "clear_agent_links": True,
    },
}


@dataclass
class ScrubSummary:
    preset: str
    pseudonym_salt: str
    agents_total: int
    agents_pseudonymized: int
    ack_flags_cleared: int
    recipients_cleared: int
    file_reservations_removed: int
    agent_links_removed: int
    secrets_replaced: int
    attachments_sanitized: int
    bodies_redacted: int


@dataclass
class ProjectRecord:
    id: int
    slug: str
    human_key: str


@dataclass
class ProjectScopeResult:
    projects: list[ProjectRecord]
    removed_count: int


def _run_am(args: list[str], *, stdin_text: str | None = None) -> str:
    env = dict(os.environ)
    env["AM_INTERFACE_MODE"] = "cli"
    proc = subprocess.run(
        [AM_BIN, *args],
        input=stdin_text,
        text=True,
        capture_output=True,
        env=env,
        check=False,
    )
    if proc.returncode != 0:
        raise RuntimeError(
            f"command failed ({proc.returncode}): {AM_BIN} {' '.join(args)}\n"
            f"stdout:\n{proc.stdout}\n"
            f"stderr:\n{proc.stderr}"
        )
    return proc.stdout


def _db_exec(db_path: Path, script: str) -> None:
    _run_am(["tooling", "db-exec", "--db", str(db_path)], stdin_text=script)


def _db_query_json(db_path: Path, sql: str) -> list[dict]:
    output = _run_am(
        ["tooling", "db-query", "--db", str(db_path), "--sql", sql, "--json"]
    ).strip()
    if not output:
        return []
    payload = json.loads(output)
    if not isinstance(payload, list):
        raise RuntimeError(f"expected JSON row array for query: {sql}")
    return payload


def _db_query_first(db_path: Path, sql: str) -> str:
    return _run_am(
        ["tooling", "db-query", "--db", str(db_path), "--sql", sql, "--first"]
    ).strip()


def _sql_quote(value) -> str:
    if value is None:
        return "NULL"
    if isinstance(value, str):
        return "'" + value.replace("'", "''") + "'"
    if isinstance(value, bool):
        return "1" if value else "0"
    if isinstance(value, (int, float)):
        return str(value)
    return "'" + json.dumps(value, sort_keys=True).replace("'", "''") + "'"


def _sha256_file(path: Path) -> str:
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def _format_in_clause(values: list[int]) -> str:
    return ",".join(str(int(value)) for value in values)


def _scrub_text(value: str) -> tuple[str, int]:
    replacements = 0
    updated = value
    for pattern in SECRET_PATTERNS:
        updated, count = pattern.subn("[REDACTED]", updated)
        replacements += count
    return updated, replacements


def _scrub_structure(value):
    if isinstance(value, str):
        new_value, replacements = _scrub_text(value)
        return new_value, replacements, 0
    if isinstance(value, list):
        total_replacements = 0
        total_removed = 0
        sanitized_list = []
        for item in value:
            sanitized_item, item_replacements, item_removed = _scrub_structure(item)
            sanitized_list.append(sanitized_item)
            total_replacements += item_replacements
            total_removed += item_removed
        return sanitized_list, total_replacements, total_removed
    if isinstance(value, dict):
        total_replacements = 0
        total_removed = 0
        sanitized_dict = {}
        for key, item in value.items():
            if key in ATTACHMENT_REDACT_KEYS:
                if item not in (None, "", [], {}):
                    total_removed += 1
                continue
            sanitized_item, item_replacements, item_removed = _scrub_structure(item)
            sanitized_dict[key] = sanitized_item
            total_replacements += item_replacements
            total_removed += item_removed
        return sanitized_dict, total_replacements, total_removed
    return value, 0, 0


def _create_schema_sql() -> str:
    return """
PRAGMA foreign_keys=ON;
CREATE TABLE IF NOT EXISTS projects (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    slug TEXT NOT NULL,
    human_key TEXT NOT NULL,
    created_at TEXT NOT NULL
);
CREATE UNIQUE INDEX IF NOT EXISTS idx_projects_slug ON projects(slug);
CREATE TABLE IF NOT EXISTS agents (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    project_id INTEGER NOT NULL REFERENCES projects(id),
    name TEXT NOT NULL,
    program TEXT NOT NULL DEFAULT '',
    model TEXT NOT NULL DEFAULT '',
    task_description TEXT NOT NULL DEFAULT '',
    inception_ts TEXT NOT NULL,
    last_active_ts TEXT NOT NULL,
    attachments_policy TEXT NOT NULL DEFAULT 'auto',
    contact_policy TEXT NOT NULL DEFAULT 'auto'
);
CREATE UNIQUE INDEX IF NOT EXISTS idx_agents_project_name ON agents(project_id, name);
CREATE TABLE IF NOT EXISTS messages (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    project_id INTEGER NOT NULL REFERENCES projects(id),
    sender_id INTEGER NOT NULL REFERENCES agents(id),
    thread_id TEXT,
    subject TEXT NOT NULL DEFAULT '',
    body_md TEXT NOT NULL DEFAULT '',
    importance TEXT NOT NULL DEFAULT 'normal',
    ack_required INTEGER NOT NULL DEFAULT 0,
    created_ts TEXT NOT NULL,
    attachments TEXT NOT NULL DEFAULT '[]'
);
CREATE INDEX IF NOT EXISTS idx_messages_project_created ON messages(project_id, created_ts);
CREATE TABLE IF NOT EXISTS message_recipients (
    message_id INTEGER NOT NULL REFERENCES messages(id),
    agent_id INTEGER NOT NULL REFERENCES agents(id),
    kind TEXT NOT NULL DEFAULT 'to',
    read_ts TEXT,
    ack_ts TEXT,
    PRIMARY KEY (message_id, agent_id)
);
CREATE TABLE IF NOT EXISTS file_reservations (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    project_id INTEGER NOT NULL REFERENCES projects(id),
    agent_id INTEGER NOT NULL REFERENCES agents(id),
    path_pattern TEXT NOT NULL,
    exclusive INTEGER NOT NULL DEFAULT 1,
    reason TEXT NOT NULL DEFAULT '',
    created_ts TEXT NOT NULL,
    expires_ts TEXT NOT NULL,
    released_ts TEXT
);
CREATE TABLE IF NOT EXISTS agent_links (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    a_project_id INTEGER NOT NULL REFERENCES projects(id),
    a_agent_id INTEGER NOT NULL REFERENCES agents(id),
    b_project_id INTEGER NOT NULL REFERENCES projects(id),
    b_agent_id INTEGER NOT NULL REFERENCES agents(id),
    status TEXT NOT NULL DEFAULT 'pending',
    reason TEXT NOT NULL DEFAULT '',
    created_ts TEXT NOT NULL,
    updated_ts TEXT NOT NULL,
    expires_ts TEXT
);
CREATE TABLE IF NOT EXISTS project_sibling_suggestions (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    project_a_id INTEGER NOT NULL REFERENCES projects(id),
    project_b_id INTEGER NOT NULL REFERENCES projects(id),
    score REAL NOT NULL DEFAULT 0.0,
    status TEXT NOT NULL DEFAULT 'suggested',
    rationale TEXT NOT NULL DEFAULT '',
    created_ts TEXT NOT NULL,
    evaluated_ts TEXT NOT NULL,
    confirmed_ts TEXT,
    dismissed_ts TEXT
);
""".strip()


def create_minimal_db(path: Path) -> None:
    script = f"""
{_create_schema_sql()};
INSERT INTO projects (id, slug, human_key, created_at) VALUES (1, 'test-proj', '/data/projects/test', {_sql_quote(TS)});
INSERT INTO agents (id, project_id, name, program, model, inception_ts, last_active_ts) VALUES (1, 1, 'BlueLake', 'claude-code', 'opus-4.5', {_sql_quote(TS)}, {_sql_quote(TS)});
INSERT INTO messages (id, project_id, sender_id, thread_id, subject, body_md, importance, ack_required, created_ts, attachments)
VALUES (1, 1, 1, 'TKT-1', 'Hello World', 'This is a test message.', 'normal', 1, {_sql_quote(TS)}, '[]');
INSERT INTO message_recipients (message_id, agent_id, kind, read_ts, ack_ts) VALUES (1, 1, 'to', {_sql_quote(TS)}, {_sql_quote(TS)});
"""
    _db_exec(path, script)


def create_with_attachments_db(path: Path) -> None:
    attachments_json = json.dumps(
        [
            {
                "type": "file",
                "path": "projects/attach-proj/attachments/ab/abcdef1234567890.webp",
                "media_type": "image/webp",
                "bytes": 32000,
                "sha256": "abcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890",
            },
            {
                "type": "file",
                "path": "projects/attach-proj/attachments/ef/efefefefefefefef.png",
                "media_type": "image/png",
                "bytes": 64000,
                "sha256": "efefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefef",
            },
        ]
    )
    script = f"""
{_create_schema_sql()};
INSERT INTO projects (id, slug, human_key, created_at) VALUES (1, 'attach-proj', '/data/projects/attach', {_sql_quote(TS)});
INSERT INTO agents (id, project_id, name, program, model, inception_ts, last_active_ts) VALUES (1, 1, 'RedStone', 'codex-cli', 'gpt-5', {_sql_quote(TS)}, {_sql_quote(TS)});
INSERT INTO messages (id, project_id, sender_id, thread_id, subject, body_md, importance, ack_required, created_ts, attachments)
VALUES (1, 1, 1, 'ATTACH-1', 'Attachment payload', 'See attached files.', 'normal', 0, {_sql_quote(TS)}, {_sql_quote(attachments_json)});
INSERT INTO message_recipients (message_id, agent_id, kind, read_ts, ack_ts) VALUES (1, 1, 'to', NULL, NULL);
"""
    _db_exec(path, script)


def create_needs_scrub_db(path: Path) -> None:
    attachments_json = json.dumps(
        [
            {
                "type": "file",
                "path": "data.json",
                "media_type": "application/json",
                "bytes": 500,
                "download_url": "https://storage.example.com/download?token=abc",
                "headers": {"Authorization": "Bearer topsecret-token-123456789"},
                "signed_url": "https://storage.example.com/signed?token=abc",
                "authorization": "Bearer MyToken1234567890123456",
                "bearer_token": "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9",
            }
        ]
    )
    script = f"""
{_create_schema_sql()};
INSERT INTO projects (id, slug, human_key, created_at) VALUES (1, 'proj-alpha', '/data/projects/alpha', {_sql_quote(TS)});
INSERT INTO projects (id, slug, human_key, created_at) VALUES (2, 'proj-beta', '/data/projects/beta', {_sql_quote(TS)});
INSERT INTO agents (id, project_id, name, program, model, inception_ts, last_active_ts) VALUES (1, 1, 'BlueLake', 'claude-code', 'opus-4.5', {_sql_quote(TS)}, {_sql_quote(TS)});
INSERT INTO agents (id, project_id, name, program, model, inception_ts, last_active_ts) VALUES (2, 2, 'GreenField', 'codex-cli', 'gpt-5', {_sql_quote(TS)}, {_sql_quote(TS)});
INSERT INTO messages (id, project_id, sender_id, thread_id, subject, body_md, importance, ack_required, created_ts, attachments)
VALUES (
    1, 1, 1, 'SCRUB-1',
    'Deploy key ghp_abcdefghijklmnopqrstuvwxyz1234567890 secret',
    'Use this token: sk-abcdef0123456789012345 for API access. Also bearer MyToken1234567890123456.',
    'high', 1, {_sql_quote(TS)}, {_sql_quote(attachments_json)}
);
INSERT INTO message_recipients (message_id, agent_id, kind, read_ts, ack_ts) VALUES (1, 2, 'to', {_sql_quote(TS)}, {_sql_quote(TS)});
INSERT INTO file_reservations (id, project_id, agent_id, path_pattern, exclusive, reason, created_ts, expires_ts)
VALUES (1, 1, 1, 'src/*.rs', 1, 'editing', {_sql_quote(TS)}, '2026-01-16T12:00:00+00:00');
INSERT INTO agent_links (id, a_project_id, a_agent_id, b_project_id, b_agent_id, status, reason, created_ts, updated_ts)
VALUES (1, 1, 1, 2, 2, 'accepted', 'cross-project sync', {_sql_quote(TS)}, {_sql_quote(TS)});
INSERT INTO project_sibling_suggestions (id, project_a_id, project_b_id, score, status, rationale, created_ts, evaluated_ts)
VALUES (1, 1, 2, 0.95, 'suggested', 'shared git remote', {_sql_quote(TS)}, {_sql_quote(TS)});
"""
    _db_exec(path, script)


def scrub_snapshot(snapshot_path: Path, *, preset: str = "standard") -> ScrubSummary:
    preset_key = (preset or "standard").strip().lower()
    preset_opts = SCRUB_PRESETS[preset_key]
    clear_ack_state = bool(preset_opts.get("clear_ack_state", True))
    clear_recipients = bool(preset_opts.get("clear_recipients", True))
    clear_file_reservations = bool(preset_opts.get("clear_file_reservations", True))
    clear_agent_links = bool(preset_opts.get("clear_agent_links", True))
    scrub_secrets = bool(preset_opts.get("scrub_secrets", True))

    agents_total = int(_db_query_first(snapshot_path, "SELECT COUNT(*) FROM agents") or 0)
    ack_flags_cleared = 0
    recipients_cleared = 0
    file_reservations_removed = 0
    agent_links_removed = 0
    secrets_replaced = 0
    attachments_sanitized = 0
    bodies_redacted = 0

    script_lines: list[str] = ["BEGIN IMMEDIATE"]

    if clear_ack_state:
        ack_flags_cleared = int(
            _db_query_first(snapshot_path, "SELECT COUNT(*) FROM messages WHERE ack_required != 0")
            or 0
        )
        script_lines.append("UPDATE messages SET ack_required = 0")

    if clear_recipients:
        recipients_cleared = int(
            _db_query_first(
                snapshot_path,
                "SELECT COUNT(*) FROM message_recipients WHERE read_ts IS NOT NULL OR ack_ts IS NOT NULL",
            )
            or 0
        )
        script_lines.append("UPDATE message_recipients SET read_ts = NULL, ack_ts = NULL")

    if clear_file_reservations:
        file_reservations_removed = int(
            _db_query_first(snapshot_path, "SELECT COUNT(*) FROM file_reservations") or 0
        )
        script_lines.append("DELETE FROM file_reservations")

    if clear_agent_links:
        agent_links_removed = int(
            _db_query_first(snapshot_path, "SELECT COUNT(*) FROM agent_links") or 0
        )
        script_lines.append("DELETE FROM agent_links")

    message_rows = _db_query_json(
        snapshot_path,
        "SELECT id, subject, body_md, attachments FROM messages ORDER BY id",
    )
    for msg in message_rows:
        msg_id = int(msg["id"])
        subject_original = msg.get("subject") or ""
        body_original = msg.get("body_md") or ""
        if scrub_secrets:
            subject, subj_replacements = _scrub_text(subject_original)
            body, body_replacements = _scrub_text(body_original)
        else:
            subject = subject_original
            body = body_original
            subj_replacements = 0
            body_replacements = 0
        secrets_replaced += subj_replacements + body_replacements

        attachments_updated = False
        attachment_replacements = 0
        attachment_keys_removed = 0
        attachments_data = []
        attachments_value = msg.get("attachments")
        if attachments_value:
            if isinstance(attachments_value, str):
                try:
                    parsed = json.loads(attachments_value)
                except json.JSONDecodeError:
                    parsed = []
                    attachments_updated = True
            elif isinstance(attachments_value, list):
                parsed = attachments_value
            else:
                parsed = []
                attachments_updated = True
            attachments_data = parsed if isinstance(parsed, list) else []

        if preset_opts["drop_attachments"] and attachments_data:
            attachments_data = []
            attachments_updated = True

        if scrub_secrets and attachments_data:
            sanitized, rep_count, removed_count = _scrub_structure(attachments_data)
            attachments_data = sanitized
            attachment_replacements = rep_count
            attachment_keys_removed = removed_count
            attachments_updated = attachments_updated or rep_count > 0 or removed_count > 0
            secrets_replaced += rep_count

        if attachments_updated:
            attachments_sanitized += 1
            sanitized_json = json.dumps(attachments_data, separators=(",", ":"), sort_keys=True)
            script_lines.append(
                f"UPDATE messages SET attachments = {_sql_quote(sanitized_json)} WHERE id = {msg_id}"
            )

        if subject != subject_original:
            script_lines.append(
                f"UPDATE messages SET subject = {_sql_quote(subject)} WHERE id = {msg_id}"
            )

        if preset_opts["redact_body"]:
            redacted_body = preset_opts.get("body_placeholder") or "[Message body redacted]"
            if body_original != redacted_body:
                bodies_redacted += 1
                script_lines.append(
                    f"UPDATE messages SET body_md = {_sql_quote(redacted_body)} WHERE id = {msg_id}"
                )
        elif body != body_original:
            script_lines.append(
                f"UPDATE messages SET body_md = {_sql_quote(body)} WHERE id = {msg_id}"
            )

    script_lines.append("COMMIT")
    _db_exec(snapshot_path, ";\n".join(script_lines) + ";\n")

    return ScrubSummary(
        preset=preset_key,
        pseudonym_salt=preset_key,
        agents_total=agents_total,
        agents_pseudonymized=0,
        ack_flags_cleared=ack_flags_cleared,
        recipients_cleared=recipients_cleared,
        file_reservations_removed=file_reservations_removed,
        agent_links_removed=agent_links_removed,
        secrets_replaced=secrets_replaced,
        attachments_sanitized=attachments_sanitized,
        bodies_redacted=bodies_redacted,
    )


def apply_project_scope(snapshot_path: Path, identifiers: list[str]) -> ProjectScopeResult:
    rows = _db_query_json(
        snapshot_path, "SELECT id, slug, human_key FROM projects ORDER BY id"
    )
    if not rows:
        raise RuntimeError("Snapshot does not contain any projects.")

    projects = [
        ProjectRecord(int(row["id"]), row["slug"], row["human_key"])
        for row in rows
    ]
    if not identifiers:
        return ProjectScopeResult(projects=projects, removed_count=0)

    lookup: dict[str, ProjectRecord] = {}
    for record in projects:
        lookup[record.slug.lower()] = record
        lookup[record.human_key.lower()] = record

    selected: list[ProjectRecord] = []
    selected_ids: set[int] = set()
    for identifier in identifiers:
        key = identifier.strip().lower()
        if not key:
            continue
        found_record = lookup.get(key)
        if found_record is None:
            raise RuntimeError(f"Project identifier '{identifier}' not found.")
        if found_record.id not in selected_ids:
            selected_ids.add(found_record.id)
            selected.append(found_record)

    if not selected:
        raise RuntimeError("No matching projects found.")

    allowed_ids = sorted(selected_ids)
    disallowed_ids = [record.id for record in projects if record.id not in selected_ids]
    if not disallowed_ids:
        return ProjectScopeResult(projects=selected, removed_count=0)

    message_rows = _db_query_json(
        snapshot_path,
        f"SELECT id FROM messages WHERE project_id NOT IN ({_format_in_clause(allowed_ids)}) ORDER BY id",
    )
    message_ids = [int(row["id"]) for row in message_rows]

    script_lines = [
        "BEGIN IMMEDIATE",
        f"DELETE FROM agent_links WHERE a_project_id NOT IN ({_format_in_clause(allowed_ids)}) OR b_project_id NOT IN ({_format_in_clause(allowed_ids)})",
        f"DELETE FROM project_sibling_suggestions WHERE project_a_id NOT IN ({_format_in_clause(allowed_ids)}) OR project_b_id NOT IN ({_format_in_clause(allowed_ids)})",
    ]
    if message_ids:
        script_lines.append(
            f"DELETE FROM message_recipients WHERE message_id IN ({_format_in_clause(message_ids)})"
        )
    script_lines.extend(
        [
            f"DELETE FROM messages WHERE project_id NOT IN ({_format_in_clause(allowed_ids)})",
            f"DELETE FROM file_reservations WHERE project_id NOT IN ({_format_in_clause(allowed_ids)})",
            f"DELETE FROM agents WHERE project_id NOT IN ({_format_in_clause(allowed_ids)})",
            f"DELETE FROM projects WHERE id NOT IN ({_format_in_clause(allowed_ids)})",
            "COMMIT",
        ]
    )
    _db_exec(snapshot_path, ";\n".join(script_lines) + ";\n")
    return ProjectScopeResult(projects=selected, removed_count=len(disallowed_ids))


def finalize_snapshot_for_export(snapshot_path: Path) -> None:
    _db_exec(
        snapshot_path,
        """
PRAGMA journal_mode=DELETE;
PRAGMA page_size=1024;
VACUUM;
PRAGMA analysis_limit=400;
ANALYZE;
PRAGMA optimize;
""".strip()
        + "\n",
    )


def run_scrub_test(db_path: Path, preset: str) -> dict:
    with tempfile.TemporaryDirectory() as tmp:
        copy_path = Path(tmp) / "scrubbed.sqlite3"
        shutil.copy2(str(db_path), str(copy_path))
        summary = scrub_snapshot(copy_path, preset=preset)
        finalize_snapshot_for_export(copy_path)
        db_hash = _sha256_file(copy_path)
        messages = _db_query_json(
            copy_path,
            "SELECT id, subject, body_md, attachments, ack_required FROM messages ORDER BY id",
        )
        recipients = _db_query_json(
            copy_path,
            "SELECT message_id, agent_id, read_ts, ack_ts FROM message_recipients ORDER BY message_id, agent_id",
        )
        file_res_count = int(_db_query_first(copy_path, "SELECT COUNT(*) FROM file_reservations") or 0)
        agent_links_count = int(_db_query_first(copy_path, "SELECT COUNT(*) FROM agent_links") or 0)
        return {
            "preset": preset,
            "summary": asdict(summary),
            "db_sha256_after_finalize": db_hash,
            "messages_after": messages,
            "recipients_after": recipients,
            "file_reservations_remaining": file_res_count,
            "agent_links_remaining": agent_links_count,
        }


def run_scope_test(db_path: Path, identifiers: list[str]) -> dict:
    with tempfile.TemporaryDirectory() as tmp:
        copy_path = Path(tmp) / "scoped.sqlite3"
        shutil.copy2(str(db_path), str(copy_path))
        result = apply_project_scope(copy_path, identifiers)
        remaining = {
            "projects": int(_db_query_first(copy_path, "SELECT COUNT(*) FROM projects") or 0),
            "agents": int(_db_query_first(copy_path, "SELECT COUNT(*) FROM agents") or 0),
            "messages": int(_db_query_first(copy_path, "SELECT COUNT(*) FROM messages") or 0),
            "recipients": int(_db_query_first(copy_path, "SELECT COUNT(*) FROM message_recipients") or 0),
            "file_reservations": int(_db_query_first(copy_path, "SELECT COUNT(*) FROM file_reservations") or 0),
            "agent_links": int(_db_query_first(copy_path, "SELECT COUNT(*) FROM agent_links") or 0),
            "project_sibling_suggestions": int(_db_query_first(copy_path, "SELECT COUNT(*) FROM project_sibling_suggestions") or 0),
        }
        return {
            "identifiers": identifiers,
            "projects": [
                {"id": p.id, "slug": p.slug, "human_key": p.human_key}
                for p in result.projects
            ],
            "removed_count": result.removed_count,
            "remaining": remaining,
        }


def main() -> None:
    os.makedirs(OUTPUT_DIR, exist_ok=True)

    minimal_path = OUTPUT_DIR / "minimal.sqlite3"
    attachments_path = OUTPUT_DIR / "with_attachments.sqlite3"
    needs_scrub_path = OUTPUT_DIR / "needs_scrub.sqlite3"

    for p in [minimal_path, attachments_path, needs_scrub_path]:
        if p.exists():
            p.unlink()

    print("Creating minimal.sqlite3...")
    create_minimal_db(minimal_path)

    print("Creating with_attachments.sqlite3...")
    create_with_attachments_db(attachments_path)

    print("Creating needs_scrub.sqlite3...")
    create_needs_scrub_db(needs_scrub_path)

    for preset in ["standard", "strict", "archive"]:
        print(f"Running scrub preset '{preset}'...")
        result = run_scrub_test(needs_scrub_path, preset)
        out_path = OUTPUT_DIR / f"expected_{preset}.json"
        out_path.write_text(json.dumps(result, indent=2, sort_keys=True) + "\n", encoding="utf-8")
        print(f"  -> {out_path.name}")

    print("Running project scope test...")
    scope_result = run_scope_test(needs_scrub_path, ["proj-alpha"])
    scope_path = OUTPUT_DIR / "expected_scoped.json"
    scope_path.write_text(
        json.dumps(scope_result, indent=2, sort_keys=True) + "\n",
        encoding="utf-8",
    )
    print(f"  -> {scope_path.name}")

    fts_ddl = """-- FTS5 virtual table for share/export search
CREATE VIRTUAL TABLE IF NOT EXISTS fts_messages USING fts5(
    subject,
    body,
    importance UNINDEXED,
    project_slug UNINDEXED,
    thread_key UNINDEXED,
    created_ts UNINDEXED
);

-- Populate from messages + projects
INSERT INTO fts_messages(rowid, subject, body, importance, project_slug, thread_key, created_ts)
SELECT
    m.id,
    COALESCE(m.subject, ''),
    COALESCE(m.body_md, ''),
    COALESCE(m.importance, ''),
    COALESCE(p.slug, ''),
    CASE
        WHEN m.thread_id IS NULL OR m.thread_id = '' THEN printf('msg:%d', m.id)
        ELSE m.thread_id
    END,
    COALESCE(m.created_ts, '')
FROM messages AS m
LEFT JOIN projects AS p ON p.id = m.project_id;

INSERT INTO fts_messages(fts_messages) VALUES('optimize');
"""
    (OUTPUT_DIR / "expected_fts_ddl.sql").write_text(fts_ddl, encoding="utf-8")

    views_ddl = """-- message_overview_mv: Denormalized message list with sender info
DROP TABLE IF EXISTS message_overview_mv;
CREATE TABLE message_overview_mv AS
SELECT
    m.id,
    m.project_id,
    m.thread_id,
    m.subject,
    m.importance,
    m.created_ts,
    a.name AS sender_name,
    LENGTH(m.body_md) AS body_length,
    json_array_length(m.attachments) AS attachment_count,
    SUBSTR(COALESCE(m.body_md, ''), 1, 280) AS latest_snippet,
    COALESCE(r.recipients, '') AS recipients
FROM messages m
JOIN agents a ON m.sender_id = a.id
LEFT JOIN (
    SELECT
        mr.message_id,
        GROUP_CONCAT(COALESCE(ag.name, ''), ', ') AS recipients
    FROM message_recipients mr
    LEFT JOIN agents ag ON ag.id = mr.agent_id
    GROUP BY mr.message_id
) r ON r.message_id = m.id
ORDER BY m.created_ts DESC;

CREATE INDEX idx_msg_overview_created ON message_overview_mv(created_ts DESC);
CREATE INDEX idx_msg_overview_thread ON message_overview_mv(thread_id, created_ts DESC);
CREATE INDEX idx_msg_overview_project ON message_overview_mv(project_id, created_ts DESC);
CREATE INDEX idx_msg_overview_importance ON message_overview_mv(importance, created_ts DESC);

-- attachments_by_message_mv: Flattened JSON attachments
DROP TABLE IF EXISTS attachments_by_message_mv;
CREATE TABLE attachments_by_message_mv AS
SELECT
    m.id AS message_id,
    json_extract(value, '$.type') AS attachment_type,
    json_extract(value, '$.media_type') AS media_type,
    json_extract(value, '$.path') AS path,
    CAST(json_extract(value, '$.bytes') AS INTEGER) AS size_bytes
FROM messages m,
     json_each(m.attachments)
WHERE m.attachments != '[]';

CREATE INDEX idx_att_by_msg ON attachments_by_message_mv(message_id);
CREATE INDEX idx_att_media_type ON attachments_by_message_mv(media_type);

-- fts_search_overview_mv: Pre-computed search result snippets (requires FTS5)
DROP TABLE IF EXISTS fts_search_overview_mv;
CREATE TABLE fts_search_overview_mv AS
SELECT
    m.id AS message_id,
    COALESCE(m.thread_id, printf('msg:%d', m.id)) AS thread_key,
    p.slug AS project_slug,
    a.name AS sender_name,
    m.importance,
    m.created_ts,
    SUBSTR(m.body_md, 1, 200) AS snippet
FROM messages m
JOIN agents a ON m.sender_id = a.id
JOIN projects p ON p.id = m.project_id;
"""
    (OUTPUT_DIR / "expected_views_ddl.sql").write_text(views_ddl, encoding="utf-8")

    hashes = {
        "minimal": _sha256_file(minimal_path),
        "with_attachments": _sha256_file(attachments_path),
        "needs_scrub": _sha256_file(needs_scrub_path),
    }
    (OUTPUT_DIR / "source_db_hashes.json").write_text(
        json.dumps(hashes, indent=2, sort_keys=True) + "\n", encoding="utf-8"
    )

    print(f"\nAll fixtures generated at {OUTPUT_DIR}")


if __name__ == "__main__":
    main()
