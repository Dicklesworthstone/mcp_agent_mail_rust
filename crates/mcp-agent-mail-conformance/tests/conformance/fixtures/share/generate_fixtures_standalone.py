#!/usr/bin/env python3
"""Generate share/export conformance fixtures (standalone, no legacy deps needed).

Usage (from the repo root):
    python3 crates/mcp-agent-mail-conformance/tests/conformance/fixtures/share/generate_fixtures_standalone.py

This script embeds the core scrub/scope logic from the legacy Python module
so it can run without sqlalchemy or other dependencies.
"""

from __future__ import annotations

import hashlib
import json
import os
import re
import shutil
import sqlite3
import tempfile
from dataclasses import asdict, dataclass
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Optional, Sequence

SCRIPT_DIR = Path(__file__).resolve().parent
OUTPUT_DIR = SCRIPT_DIR

# ---- Constants (verbatim from legacy share.py) ----

SECRET_PATTERNS = (
    re.compile(r"ghp_[A-Za-z0-9]{36,}", re.IGNORECASE),
    re.compile(r"github_pat_[A-Za-z0-9_]{20,}", re.IGNORECASE),
    re.compile(r"xox[baprs]-[A-Za-z0-9-]{10,}", re.IGNORECASE),
    re.compile(r"sk-[A-Za-z0-9]{20,}", re.IGNORECASE),
    re.compile(r"(?i)bearer\s+[A-Za-z0-9_\-\.]{16,}"),
    re.compile(r"eyJ[0-9A-Za-z_-]+\.[0-9A-Za-z_-]+\.[0-9A-Za-z_-]+"),
)

ATTACHMENT_REDACT_KEYS = frozenset({
    "download_url", "headers", "authorization", "signed_url", "bearer_token",
})

SCRUB_PRESETS = {
    "standard": {
        "redact_body": False, "body_placeholder": None, "drop_attachments": False,
        "scrub_secrets": True, "clear_ack_state": True, "clear_recipients": True,
        "clear_file_reservations": True, "clear_agent_links": True,
    },
    "strict": {
        "redact_body": True, "body_placeholder": "[Message body redacted]",
        "drop_attachments": True, "scrub_secrets": True, "clear_ack_state": True,
        "clear_recipients": True, "clear_file_reservations": True, "clear_agent_links": True,
    },
    "archive": {
        "redact_body": False, "body_placeholder": None, "drop_attachments": False,
        "scrub_secrets": False, "clear_ack_state": False, "clear_recipients": False,
        "clear_file_reservations": False, "clear_agent_links": False,
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
    attachments_cleared: int


@dataclass
class ProjectRecord:
    id: int
    slug: str
    human_key: str


@dataclass
class ProjectScopeResult:
    projects: list
    removed_count: int


# ---- Core logic (verbatim from legacy share.py) ----

def _scrub_text(value: str) -> tuple:
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


def scrub_snapshot(snapshot_path, *, preset="standard"):
    preset_key = (preset or "standard").strip().lower()
    preset_opts = SCRUB_PRESETS[preset_key]
    clear_ack_state = bool(preset_opts.get("clear_ack_state", True))
    clear_recipients = bool(preset_opts.get("clear_recipients", True))
    clear_file_reservations = bool(preset_opts.get("clear_file_reservations", True))
    clear_agent_links = bool(preset_opts.get("clear_agent_links", True))
    scrub_secrets = bool(preset_opts.get("scrub_secrets", True))

    bodies_redacted = 0
    attachments_cleared = 0

    conn = sqlite3.connect(str(snapshot_path))
    try:
        conn.row_factory = sqlite3.Row
        conn.execute("PRAGMA foreign_keys=ON")

        agents_total = conn.execute("SELECT COUNT(*) FROM agents").fetchone()[0]
        agents_pseudonymized = 0

        if clear_ack_state:
            ack_cursor = conn.execute("UPDATE messages SET ack_required = 0")
            ack_flags_cleared = ack_cursor.rowcount or 0
        else:
            ack_flags_cleared = 0

        if clear_recipients:
            recipients_cursor = conn.execute("UPDATE message_recipients SET read_ts = NULL, ack_ts = NULL")
            recipients_cleared = recipients_cursor.rowcount or 0
        else:
            recipients_cleared = 0

        if clear_file_reservations:
            file_res_cursor = conn.execute("DELETE FROM file_reservations")
            file_res_removed = file_res_cursor.rowcount or 0
        else:
            file_res_removed = 0

        if clear_agent_links:
            agent_links_cursor = conn.execute("DELETE FROM agent_links")
            agent_links_removed = agent_links_cursor.rowcount or 0
        else:
            agent_links_removed = 0

        secrets_replaced = 0
        attachments_sanitized = 0

        message_rows = conn.execute("SELECT id, subject, body_md, attachments FROM messages").fetchall()
        for msg in message_rows:
            subject_original = msg["subject"] or ""
            body_original = msg["body_md"] or ""
            if scrub_secrets:
                subject, subj_replacements = _scrub_text(subject_original)
                body, body_replacements = _scrub_text(body_original)
            else:
                subject = subject_original
                body = body_original
                subj_replacements = 0
                body_replacements = 0
            secrets_replaced += subj_replacements + body_replacements
            attachments_value = msg["attachments"]
            attachments_updated = False
            attachment_replacements = 0
            attachment_keys_removed = 0
            if attachments_value:
                if isinstance(attachments_value, str):
                    try:
                        parsed = json.loads(attachments_value)
                    except json.JSONDecodeError:
                        parsed = []
                        attachments_updated = True
                    if isinstance(parsed, list):
                        attachments_data = parsed
                    else:
                        attachments_data = []
                        attachments_updated = True
                elif isinstance(attachments_value, list):
                    attachments_data = attachments_value
                else:
                    attachments_data = []
                    attachments_updated = True
            else:
                attachments_data = []
            if preset_opts["drop_attachments"] and attachments_data:
                attachments_data = []
                attachments_cleared += 1
                attachments_updated = True
            if scrub_secrets and attachments_data:
                sanitized, rep_count, removed_count = _scrub_structure(attachments_data)
                attachment_replacements += rep_count
                attachment_keys_removed += removed_count
                if sanitized != attachments_data:
                    attachments_data = sanitized
                    attachments_updated = True
            if attachments_updated:
                sanitized_json = json.dumps(attachments_data, separators=(",", ":"), sort_keys=True)
                conn.execute("UPDATE messages SET attachments = ? WHERE id = ?", (sanitized_json, msg["id"]))
            if subject != msg["subject"]:
                conn.execute("UPDATE messages SET subject = ? WHERE id = ?", (subject, msg["id"]))
            if preset_opts["redact_body"]:
                body = preset_opts.get("body_placeholder") or "[Message body redacted]"
                if msg["body_md"] != body:
                    bodies_redacted += 1
                    conn.execute("UPDATE messages SET body_md = ? WHERE id = ?", (body, msg["id"]))
            elif body != msg["body_md"]:
                conn.execute("UPDATE messages SET body_md = ? WHERE id = ?", (body, msg["id"]))
            secrets_replaced += attachment_replacements
            if attachments_updated or attachment_replacements or attachment_keys_removed:
                attachments_sanitized += 1

        conn.commit()
    finally:
        conn.close()

    return ScrubSummary(
        preset=preset_key,
        pseudonym_salt=preset_key,
        agents_total=agents_total,
        agents_pseudonymized=int(agents_pseudonymized),
        ack_flags_cleared=ack_flags_cleared,
        recipients_cleared=recipients_cleared,
        file_reservations_removed=file_res_removed,
        agent_links_removed=agent_links_removed,
        secrets_replaced=secrets_replaced,
        attachments_sanitized=attachments_sanitized,
        bodies_redacted=bodies_redacted,
        attachments_cleared=attachments_cleared,
    )


def _format_in_clause(count):
    return ",".join("?" for _ in range(count))


def apply_project_scope(snapshot_path, identifiers):
    conn = sqlite3.connect(str(snapshot_path))
    try:
        conn.row_factory = sqlite3.Row
        conn.execute("PRAGMA foreign_keys=ON")

        rows = conn.execute("SELECT id, slug, human_key FROM projects").fetchall()
        if not rows:
            raise RuntimeError("Snapshot does not contain any projects.")

        projects = [ProjectRecord(int(row["id"]), row["slug"], row["human_key"]) for row in rows]

        if not identifiers:
            return ProjectScopeResult(projects=projects, removed_count=0)

        lookup = {}
        for record in projects:
            lookup[record.slug.lower()] = record
            lookup[record.human_key.lower()] = record

        selected = []
        selected_ids = set()
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

        allowed_ids = {record.id for record in selected}
        disallowed_ids = [record.id for record in projects if record.id not in allowed_ids]
        if not disallowed_ids:
            return ProjectScopeResult(projects=selected, removed_count=0)

        placeholders = _format_in_clause(len(allowed_ids))
        params = tuple(allowed_ids)

        conn.execute(
            f"DELETE FROM agent_links WHERE a_project_id NOT IN ({placeholders}) OR b_project_id NOT IN ({placeholders})",
            params + params,
        )
        conn.execute(
            f"DELETE FROM project_sibling_suggestions WHERE project_a_id NOT IN ({placeholders}) OR project_b_id NOT IN ({placeholders})",
            params + params,
        )
        to_remove_messages = conn.execute(
            f"SELECT id FROM messages WHERE project_id NOT IN ({placeholders})", params
        ).fetchall()
        if to_remove_messages:
            msg_placeholders = _format_in_clause(len(to_remove_messages))
            conn.execute(
                f"DELETE FROM message_recipients WHERE message_id IN ({msg_placeholders})",
                tuple(int(row["id"]) for row in to_remove_messages),
            )
        conn.execute(f"DELETE FROM messages WHERE project_id NOT IN ({placeholders})", params)
        conn.execute(f"DELETE FROM file_reservations WHERE project_id NOT IN ({placeholders})", params)
        conn.execute(f"DELETE FROM agents WHERE project_id NOT IN ({placeholders})", params)
        conn.execute(f"DELETE FROM projects WHERE id NOT IN ({placeholders})", params)
        conn.commit()

        return ProjectScopeResult(projects=selected, removed_count=len(disallowed_ids))
    finally:
        conn.close()


def finalize_snapshot_for_export(snapshot_path):
    conn = sqlite3.connect(str(snapshot_path))
    try:
        conn.execute("PRAGMA journal_mode=DELETE")
        conn.execute("PRAGMA page_size=1024")
        conn.execute("VACUUM")
        conn.execute("PRAGMA analysis_limit=400")
        conn.execute("ANALYZE")
        conn.execute("PRAGMA optimize")
        conn.commit()
    finally:
        conn.close()


# ---- DB Creation ----

def _create_schema(conn):
    conn.executescript("""
        CREATE TABLE IF NOT EXISTS projects (
            id INTEGER PRIMARY KEY AUTOINCREMENT, slug TEXT NOT NULL, human_key TEXT NOT NULL, created_at TEXT NOT NULL
        );
        CREATE UNIQUE INDEX IF NOT EXISTS idx_projects_slug ON projects(slug);
        CREATE TABLE IF NOT EXISTS agents (
            id INTEGER PRIMARY KEY AUTOINCREMENT, project_id INTEGER NOT NULL REFERENCES projects(id),
            name TEXT NOT NULL, program TEXT NOT NULL DEFAULT '', model TEXT NOT NULL DEFAULT '',
            task_description TEXT NOT NULL DEFAULT '', inception_ts TEXT NOT NULL, last_active_ts TEXT NOT NULL,
            attachments_policy TEXT NOT NULL DEFAULT 'auto', contact_policy TEXT NOT NULL DEFAULT 'auto'
        );
        CREATE UNIQUE INDEX IF NOT EXISTS idx_agents_project_name ON agents(project_id, name);
        CREATE TABLE IF NOT EXISTS messages (
            id INTEGER PRIMARY KEY AUTOINCREMENT, project_id INTEGER NOT NULL REFERENCES projects(id),
            sender_id INTEGER NOT NULL REFERENCES agents(id), thread_id TEXT, subject TEXT NOT NULL DEFAULT '',
            body_md TEXT NOT NULL DEFAULT '', importance TEXT NOT NULL DEFAULT 'normal',
            ack_required INTEGER NOT NULL DEFAULT 0, created_ts TEXT NOT NULL, attachments TEXT NOT NULL DEFAULT '[]'
        );
        CREATE INDEX IF NOT EXISTS idx_messages_project_created ON messages(project_id, created_ts);
        CREATE TABLE IF NOT EXISTS message_recipients (
            message_id INTEGER NOT NULL REFERENCES messages(id), agent_id INTEGER NOT NULL REFERENCES agents(id),
            kind TEXT NOT NULL DEFAULT 'to', read_ts TEXT, ack_ts TEXT, PRIMARY KEY (message_id, agent_id)
        );
        CREATE TABLE IF NOT EXISTS file_reservations (
            id INTEGER PRIMARY KEY AUTOINCREMENT, project_id INTEGER NOT NULL REFERENCES projects(id),
            agent_id INTEGER NOT NULL REFERENCES agents(id), path_pattern TEXT NOT NULL,
            exclusive INTEGER NOT NULL DEFAULT 1, reason TEXT NOT NULL DEFAULT '', created_ts TEXT NOT NULL,
            expires_ts TEXT NOT NULL, released_ts TEXT
        );
        CREATE TABLE IF NOT EXISTS agent_links (
            id INTEGER PRIMARY KEY AUTOINCREMENT, a_project_id INTEGER NOT NULL REFERENCES projects(id),
            a_agent_id INTEGER NOT NULL REFERENCES agents(id), b_project_id INTEGER NOT NULL REFERENCES projects(id),
            b_agent_id INTEGER NOT NULL REFERENCES agents(id), status TEXT NOT NULL DEFAULT 'pending',
            reason TEXT NOT NULL DEFAULT '', created_ts TEXT NOT NULL, updated_ts TEXT NOT NULL, expires_ts TEXT
        );
        CREATE TABLE IF NOT EXISTS project_sibling_suggestions (
            id INTEGER PRIMARY KEY AUTOINCREMENT, project_a_id INTEGER NOT NULL REFERENCES projects(id),
            project_b_id INTEGER NOT NULL REFERENCES projects(id), score REAL NOT NULL DEFAULT 0.0,
            status TEXT NOT NULL DEFAULT 'suggested', rationale TEXT NOT NULL DEFAULT '', created_ts TEXT NOT NULL,
            evaluated_ts TEXT NOT NULL, confirmed_ts TEXT, dismissed_ts TEXT
        );
    """)
    conn.commit()


TS = "2026-01-15T12:00:00+00:00"


def _sha256_file(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def create_minimal_db(path):
    conn = sqlite3.connect(str(path))
    _create_schema(conn)
    conn.execute("INSERT INTO projects (id, slug, human_key, created_at) VALUES (1, 'test-proj', '/data/projects/test', ?)", (TS,))
    conn.execute("INSERT INTO agents (id, project_id, name, program, model, inception_ts, last_active_ts) VALUES (1, 1, 'BlueLake', 'claude-code', 'opus-4.5', ?, ?)", (TS, TS))
    conn.execute("INSERT INTO messages (id, project_id, sender_id, thread_id, subject, body_md, importance, ack_required, created_ts, attachments) VALUES (1, 1, 1, 'TKT-1', 'Hello World', 'This is a test message.', 'normal', 1, ?, '[]')", (TS,))
    conn.execute("INSERT INTO message_recipients (message_id, agent_id, kind, read_ts, ack_ts) VALUES (1, 1, 'to', ?, ?)", (TS, TS))
    conn.commit()
    conn.close()


def create_with_attachments_db(path):
    conn = sqlite3.connect(str(path))
    _create_schema(conn)
    conn.execute("INSERT INTO projects (id, slug, human_key, created_at) VALUES (1, 'attach-proj', '/data/projects/attach', ?)", (TS,))
    conn.execute("INSERT INTO agents (id, project_id, name, program, model, inception_ts, last_active_ts) VALUES (1, 1, 'RedStone', 'codex-cli', 'gpt-5', ?, ?)", (TS, TS))
    att = json.dumps([
        {"type": "file", "path": "projects/attach-proj/attachments/ab/abcdef1234567890.webp", "media_type": "image/webp", "bytes": 32000, "sha256": "abcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890"},
        {"type": "file", "path": "projects/attach-proj/attachments/cd/cdef0123456789ab.png", "media_type": "image/png", "bytes": 100000, "sha256": "cdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789ab", "download_url": "https://example.com/secret-download", "headers": {"Authorization": "Bearer sk-secret123456789012345"}},
    ])
    conn.execute("INSERT INTO messages (id, project_id, sender_id, thread_id, subject, body_md, importance, ack_required, created_ts, attachments) VALUES (1, 1, 1, 'TKT-A1', 'Screenshot review', 'Please review the attached screenshots.', 'high', 1, ?, ?)", (TS, att))
    conn.execute("INSERT INTO message_recipients (message_id, agent_id, kind, read_ts, ack_ts) VALUES (1, 1, 'to', ?, NULL)", (TS,))
    conn.commit()
    conn.close()


def create_needs_scrub_db(path):
    conn = sqlite3.connect(str(path))
    _create_schema(conn)
    conn.execute("INSERT INTO projects (id, slug, human_key, created_at) VALUES (1, 'proj-alpha', '/data/projects/alpha', ?)", (TS,))
    conn.execute("INSERT INTO projects (id, slug, human_key, created_at) VALUES (2, 'proj-beta', '/data/projects/beta', ?)", (TS,))
    conn.execute("INSERT INTO agents (id, project_id, name, program, model, inception_ts, last_active_ts) VALUES (1, 1, 'GreenCastle', 'claude-code', 'opus-4.5', ?, ?)", (TS, TS))
    conn.execute("INSERT INTO agents (id, project_id, name, program, model, inception_ts, last_active_ts) VALUES (2, 2, 'PurpleBear', 'codex-cli', 'gpt-5', ?, ?)", (TS, TS))

    conn.execute("INSERT INTO messages (id, project_id, sender_id, thread_id, subject, body_md, importance, ack_required, created_ts, attachments) VALUES (1, 1, 1, 'TKT-S1', 'Deploy key: ghp_aBcDeFgHiJkLmNoPqRsTuVwXyZ0123456789', 'Use this token: sk-abcdef0123456789012345 for API access. Also bearer MyToken1234567890123456.', 'urgent', 1, ?, '[]')", (TS,))
    conn.execute("INSERT INTO messages (id, project_id, sender_id, thread_id, subject, body_md, importance, ack_required, created_ts, attachments) VALUES (2, 1, 1, 'TKT-S2', 'Auth token refresh', 'New JWT: eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dozjgNryP4J3jVmNHl0w5N_XgL0n3I9PlFUP0THsR8U', 'normal', 0, ?, '[]')", (TS,))

    secret_att = json.dumps([{
        "type": "file", "path": "data.json", "media_type": "application/json", "bytes": 500,
        "download_url": "https://secret.example.com/file", "signed_url": "https://storage.example.com/signed?token=abc",
        "authorization": "Bearer xoxb-1234567890-abcdefghij", "bearer_token": "ghp_AAAAABBBBCCCCDDDDEEEEFFFFFGGGG12345",
    }])
    conn.execute("INSERT INTO messages (id, project_id, sender_id, thread_id, subject, body_md, importance, ack_required, created_ts, attachments) VALUES (3, 2, 2, 'TKT-B1', 'Credentials for staging', 'Slack token: xoxb-1234567890-abcdefghij and PAT: github_pat_abcdefghijklmnopqrstuvwxyz', 'normal', 1, ?, ?)", (TS, secret_att))

    conn.execute("INSERT INTO message_recipients (message_id, agent_id, kind, read_ts, ack_ts) VALUES (1, 1, 'to', ?, ?)", (TS, TS))
    conn.execute("INSERT INTO message_recipients (message_id, agent_id, kind, read_ts, ack_ts) VALUES (2, 1, 'to', ?, NULL)", (TS,))
    conn.execute("INSERT INTO message_recipients (message_id, agent_id, kind, read_ts, ack_ts) VALUES (3, 2, 'to', NULL, NULL)")
    conn.execute("INSERT INTO file_reservations (id, project_id, agent_id, path_pattern, exclusive, reason, created_ts, expires_ts) VALUES (1, 1, 1, 'src/*.rs', 1, 'editing', ?, '2026-01-16T12:00:00+00:00')", (TS,))
    conn.execute("INSERT INTO agent_links (id, a_project_id, a_agent_id, b_project_id, b_agent_id, status, reason, created_ts, updated_ts) VALUES (1, 1, 1, 2, 2, 'approved', 'collaboration', ?, ?)", (TS, TS))
    conn.execute("INSERT INTO project_sibling_suggestions (id, project_a_id, project_b_id, score, status, rationale, created_ts, evaluated_ts) VALUES (1, 1, 2, 0.85, 'suggested', 'shared domain', ?, ?)", (TS, TS))
    conn.commit()
    conn.close()


def run_scrub_test(db_path, preset):
    with tempfile.TemporaryDirectory() as tmp:
        copy_path = Path(tmp) / "scrubbed.sqlite3"
        shutil.copy2(str(db_path), str(copy_path))
        summary = scrub_snapshot(copy_path, preset=preset)
        finalize_snapshot_for_export(copy_path)
        db_hash = _sha256_file(copy_path)
        conn = sqlite3.connect(str(copy_path))
        conn.row_factory = sqlite3.Row
        messages = []
        for row in conn.execute("SELECT id, subject, body_md, attachments, ack_required FROM messages ORDER BY id"):
            messages.append({"id": row["id"], "subject": row["subject"], "body_md": row["body_md"], "attachments": row["attachments"], "ack_required": row["ack_required"]})
        recipients = []
        for row in conn.execute("SELECT message_id, agent_id, read_ts, ack_ts FROM message_recipients ORDER BY message_id, agent_id"):
            recipients.append({"message_id": row["message_id"], "agent_id": row["agent_id"], "read_ts": row["read_ts"], "ack_ts": row["ack_ts"]})
        file_res_count = conn.execute("SELECT COUNT(*) FROM file_reservations").fetchone()[0]
        agent_links_count = conn.execute("SELECT COUNT(*) FROM agent_links").fetchone()[0]
        conn.close()
        return {
            "preset": preset,
            "summary": asdict(summary),
            "db_sha256_after_finalize": db_hash,
            "messages_after": messages,
            "recipients_after": recipients,
            "file_reservations_remaining": file_res_count,
            "agent_links_remaining": agent_links_count,
        }


def run_scope_test(db_path, identifiers):
    with tempfile.TemporaryDirectory() as tmp:
        copy_path = Path(tmp) / "scoped.sqlite3"
        shutil.copy2(str(db_path), str(copy_path))
        result = apply_project_scope(copy_path, identifiers)
        conn = sqlite3.connect(str(copy_path))
        remaining = {
            "projects": conn.execute("SELECT COUNT(*) FROM projects").fetchone()[0],
            "agents": conn.execute("SELECT COUNT(*) FROM agents").fetchone()[0],
            "messages": conn.execute("SELECT COUNT(*) FROM messages").fetchone()[0],
            "recipients": conn.execute("SELECT COUNT(*) FROM message_recipients").fetchone()[0],
            "file_reservations": conn.execute("SELECT COUNT(*) FROM file_reservations").fetchone()[0],
            "agent_links": conn.execute("SELECT COUNT(*) FROM agent_links").fetchone()[0],
            "project_sibling_suggestions": conn.execute("SELECT COUNT(*) FROM project_sibling_suggestions").fetchone()[0],
        }
        conn.close()
        return {
            "identifiers": identifiers,
            "projects": [{"id": p.id, "slug": p.slug, "human_key": p.human_key} for p in result.projects],
            "removed_count": result.removed_count,
            "remaining": remaining,
        }


def main():
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
    scope_path.write_text(json.dumps(scope_result, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    print(f"  -> {scope_path.name}")

    hashes = {
        "minimal": _sha256_file(minimal_path),
        "with_attachments": _sha256_file(attachments_path),
        "needs_scrub": _sha256_file(needs_scrub_path),
    }
    (OUTPUT_DIR / "source_db_hashes.json").write_text(json.dumps(hashes, indent=2, sort_keys=True) + "\n", encoding="utf-8")

    print(f"\nAll fixtures generated at {OUTPUT_DIR}")


if __name__ == "__main__":
    main()
