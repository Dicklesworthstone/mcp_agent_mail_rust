#!/usr/bin/env python3
"""Generate deterministic share/export conformance fixtures.

Usage (from the repo root):
    cd legacy_python_mcp_agent_mail_code/mcp_agent_mail
    uv run python ../../crates/mcp-agent-mail-conformance/tests/conformance/fixtures/share/generate_share_fixtures.py

Output:
    crates/mcp-agent-mail-conformance/tests/conformance/fixtures/share/
    ├── minimal.sqlite3              — Smallest useful DB (1 project, 1 agent, 1 message)
    ├── with_attachments.sqlite3     — DB with file-type attachment metadata
    ├── needs_scrub.sqlite3          — DB with secrets embedded in subject/body/attachments
    ├── expected_standard.json       — Expected ScrubSummary + manifest fields for standard preset
    ├── expected_strict.json         — Expected ScrubSummary + manifest fields for strict preset
    ├── expected_archive.json        — Expected ScrubSummary + manifest fields for archive preset
    ├── expected_scoped.json         — Expected ProjectScopeResult for multi-project DB
    ├── expected_fts_ddl.sql         — The exact FTS5 DDL
    ├── expected_views_ddl.sql       — The exact materialized views DDL
    └── README.md                    — Regeneration instructions
"""

from __future__ import annotations

import hashlib
import json
import os
import sqlite3
import sys
import tempfile
from datetime import datetime, timezone
from pathlib import Path

# Resolve output directory
SCRIPT_DIR = Path(__file__).resolve().parent
REPO_ROOT = SCRIPT_DIR.parents[4]  # Up to repo root
OUTPUT_DIR = SCRIPT_DIR

# Ensure legacy Python module is importable
sys.path.insert(0, str(REPO_ROOT / "legacy_python_mcp_agent_mail_code" / "mcp_agent_mail" / "src"))


def _sha256_file(path: Path) -> str:
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def _create_schema(conn: sqlite3.Connection) -> None:
    """Create the full MCP Agent Mail schema."""
    conn.executescript("""
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
    """)
    conn.commit()


def _now_iso() -> str:
    return "2026-01-15T12:00:00+00:00"


def create_minimal_db(path: Path) -> None:
    """Smallest useful DB: 1 project, 1 agent, 1 message."""
    conn = sqlite3.connect(str(path))
    _create_schema(conn)
    ts = _now_iso()
    conn.execute("INSERT INTO projects (id, slug, human_key, created_at) VALUES (1, 'test-proj', '/data/projects/test', ?)", (ts,))
    conn.execute("INSERT INTO agents (id, project_id, name, program, model, inception_ts, last_active_ts) VALUES (1, 1, 'BlueLake', 'claude-code', 'opus-4.5', ?, ?)", (ts, ts))
    conn.execute("""INSERT INTO messages (id, project_id, sender_id, thread_id, subject, body_md, importance, ack_required, created_ts, attachments)
        VALUES (1, 1, 1, 'TKT-1', 'Hello World', 'This is a test message.', 'normal', 1, ?, '[]')""", (ts,))
    conn.execute("INSERT INTO message_recipients (message_id, agent_id, kind, read_ts, ack_ts) VALUES (1, 1, 'to', ?, ?)", (ts, ts))
    conn.commit()
    conn.close()


def create_with_attachments_db(path: Path) -> None:
    """DB with file-type attachment metadata."""
    conn = sqlite3.connect(str(path))
    _create_schema(conn)
    ts = _now_iso()
    conn.execute("INSERT INTO projects (id, slug, human_key, created_at) VALUES (1, 'attach-proj', '/data/projects/attach', ?)", (ts,))
    conn.execute("INSERT INTO agents (id, project_id, name, program, model, inception_ts, last_active_ts) VALUES (1, 1, 'RedStone', 'codex-cli', 'gpt-5', ?, ?)", (ts, ts))

    attachments_json = json.dumps([
        {
            "type": "file",
            "path": "projects/attach-proj/attachments/ab/abcdef1234567890.webp",
            "media_type": "image/webp",
            "bytes": 32000,
            "sha256": "abcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890",
        },
        {
            "type": "file",
            "path": "projects/attach-proj/attachments/cd/cdef0123456789ab.png",
            "media_type": "image/png",
            "bytes": 100000,
            "sha256": "cdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789ab",
            "download_url": "https://example.com/secret-download",
            "headers": {"Authorization": "Bearer sk-secret123456789012345"},
        },
    ])

    conn.execute("""INSERT INTO messages (id, project_id, sender_id, thread_id, subject, body_md, importance, ack_required, created_ts, attachments)
        VALUES (1, 1, 1, 'TKT-A1', 'Screenshot review', 'Please review the attached screenshots.', 'high', 1, ?, ?)""",
        (ts, attachments_json))
    conn.execute("INSERT INTO message_recipients (message_id, agent_id, kind, read_ts, ack_ts) VALUES (1, 1, 'to', ?, NULL)", (ts,))
    conn.commit()
    conn.close()


def create_needs_scrub_db(path: Path) -> None:
    """DB with secrets embedded in subject/body/attachments for scrub testing."""
    conn = sqlite3.connect(str(path))
    _create_schema(conn)
    ts = _now_iso()

    # Two projects for scoping tests
    conn.execute("INSERT INTO projects (id, slug, human_key, created_at) VALUES (1, 'proj-alpha', '/data/projects/alpha', ?)", (ts,))
    conn.execute("INSERT INTO projects (id, slug, human_key, created_at) VALUES (2, 'proj-beta', '/data/projects/beta', ?)", (ts,))

    conn.execute("INSERT INTO agents (id, project_id, name, program, model, inception_ts, last_active_ts) VALUES (1, 1, 'GreenCastle', 'claude-code', 'opus-4.5', ?, ?)", (ts, ts))
    conn.execute("INSERT INTO agents (id, project_id, name, program, model, inception_ts, last_active_ts) VALUES (2, 2, 'PurpleBear', 'codex-cli', 'gpt-5', ?, ?)", (ts, ts))

    # Message with GitHub PAT in subject
    conn.execute("""INSERT INTO messages (id, project_id, sender_id, thread_id, subject, body_md, importance, ack_required, created_ts, attachments)
        VALUES (1, 1, 1, 'TKT-S1', 'Deploy key: ghp_aBcDeFgHiJkLmNoPqRsTuVwXyZ0123456789',
                'Use this token: sk-abcdef0123456789012345 for API access. Also bearer MyToken1234567890123456.',
                'urgent', 1, ?, '[]')""", (ts,))

    # Message with JWT in body
    conn.execute("""INSERT INTO messages (id, project_id, sender_id, thread_id, subject, body_md, importance, ack_required, created_ts, attachments)
        VALUES (2, 1, 1, 'TKT-S2', 'Auth token refresh',
                'New JWT: eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dozjgNryP4J3jVmNHl0w5N_XgL0n3I9PlFUP0THsR8U',
                'normal', 0, ?, '[]')""", (ts,))

    # Message in project beta with attachment containing secrets
    secret_attachment = json.dumps([{
        "type": "file",
        "path": "data.json",
        "media_type": "application/json",
        "bytes": 500,
        "download_url": "https://secret.example.com/file",
        "signed_url": "https://storage.example.com/signed?token=abc",
        "authorization": "Bearer xoxb-1234567890-abcdefghij",
        "bearer_token": "ghp_AAAAABBBBCCCCDDDDEEEEFFFFFGGGG12345",
    }])
    conn.execute("""INSERT INTO messages (id, project_id, sender_id, thread_id, subject, body_md, importance, ack_required, created_ts, attachments)
        VALUES (3, 2, 2, 'TKT-B1', 'Credentials for staging',
                'Slack token: xoxb-1234567890-abcdefghij and PAT: github_pat_abcdefghijklmnopqrstuvwxyz',
                'normal', 1, ?, ?)""", (ts, secret_attachment))

    # Recipients with read/ack state
    conn.execute("INSERT INTO message_recipients (message_id, agent_id, kind, read_ts, ack_ts) VALUES (1, 1, 'to', ?, ?)", (ts, ts))
    conn.execute("INSERT INTO message_recipients (message_id, agent_id, kind, read_ts, ack_ts) VALUES (2, 1, 'to', ?, NULL)", (ts,))
    conn.execute("INSERT INTO message_recipients (message_id, agent_id, kind, read_ts, ack_ts) VALUES (3, 2, 'to', NULL, NULL)")

    # File reservations
    conn.execute("""INSERT INTO file_reservations (id, project_id, agent_id, path_pattern, exclusive, reason, created_ts, expires_ts)
        VALUES (1, 1, 1, 'src/*.rs', 1, 'editing', ?, '2026-01-16T12:00:00+00:00')""", (ts,))

    # Agent links
    conn.execute("""INSERT INTO agent_links (id, a_project_id, a_agent_id, b_project_id, b_agent_id, status, reason, created_ts, updated_ts)
        VALUES (1, 1, 1, 2, 2, 'approved', 'collaboration', ?, ?)""", (ts, ts))

    # Sibling suggestions
    conn.execute("""INSERT INTO project_sibling_suggestions (id, project_a_id, project_b_id, score, status, rationale, created_ts, evaluated_ts)
        VALUES (1, 1, 2, 0.85, 'suggested', 'shared domain', ?, ?)""", (ts, ts))

    conn.commit()
    conn.close()


def run_scrub_and_collect(db_path: Path, preset: str) -> dict:
    """Run scrub on a copy and return the summary + DB hash."""
    import shutil
    from mcp_agent_mail.share import scrub_snapshot, finalize_snapshot_for_export
    from dataclasses import asdict

    with tempfile.TemporaryDirectory() as tmp:
        copy_path = Path(tmp) / "scrubbed.sqlite3"
        shutil.copy2(str(db_path), str(copy_path))

        summary = scrub_snapshot(copy_path, preset=preset)
        finalize_snapshot_for_export(copy_path)

        db_hash = _sha256_file(copy_path)

        # Read back message state
        conn = sqlite3.connect(str(copy_path))
        conn.row_factory = sqlite3.Row
        messages = []
        for row in conn.execute("SELECT id, subject, body_md, attachments, ack_required FROM messages ORDER BY id"):
            messages.append({
                "id": row["id"],
                "subject": row["subject"],
                "body_md": row["body_md"],
                "attachments": row["attachments"],
                "ack_required": row["ack_required"],
            })
        recipients = []
        for row in conn.execute("SELECT message_id, agent_id, read_ts, ack_ts FROM message_recipients ORDER BY message_id, agent_id"):
            recipients.append({
                "message_id": row["message_id"],
                "agent_id": row["agent_id"],
                "read_ts": row["read_ts"],
                "ack_ts": row["ack_ts"],
            })
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


def run_scope_test(db_path: Path, identifiers: list[str]) -> dict:
    """Run project scoping on a copy and return the result."""
    import shutil
    from mcp_agent_mail.share import apply_project_scope
    from dataclasses import asdict

    with tempfile.TemporaryDirectory() as tmp:
        copy_path = Path(tmp) / "scoped.sqlite3"
        shutil.copy2(str(db_path), str(copy_path))

        result = apply_project_scope(copy_path, identifiers)

        conn = sqlite3.connect(str(copy_path))
        remaining_projects = conn.execute("SELECT COUNT(*) FROM projects").fetchone()[0]
        remaining_agents = conn.execute("SELECT COUNT(*) FROM agents").fetchone()[0]
        remaining_messages = conn.execute("SELECT COUNT(*) FROM messages").fetchone()[0]
        remaining_recipients = conn.execute("SELECT COUNT(*) FROM message_recipients").fetchone()[0]
        remaining_file_res = conn.execute("SELECT COUNT(*) FROM file_reservations").fetchone()[0]
        remaining_links = conn.execute("SELECT COUNT(*) FROM agent_links").fetchone()[0]
        remaining_siblings = conn.execute("SELECT COUNT(*) FROM project_sibling_suggestions").fetchone()[0]
        conn.close()

        return {
            "identifiers": identifiers,
            "projects": [{"id": p.id, "slug": p.slug, "human_key": p.human_key} for p in result.projects],
            "removed_count": result.removed_count,
            "remaining": {
                "projects": remaining_projects,
                "agents": remaining_agents,
                "messages": remaining_messages,
                "recipients": remaining_recipients,
                "file_reservations": remaining_file_res,
                "agent_links": remaining_links,
                "project_sibling_suggestions": remaining_siblings,
            },
        }


def main() -> None:
    os.makedirs(OUTPUT_DIR, exist_ok=True)

    # Step 1: Create fixture DBs
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

    # Step 2: Run scrub for each preset on needs_scrub DB
    for preset in ["standard", "strict", "archive"]:
        print(f"Running scrub preset '{preset}' on needs_scrub.sqlite3...")
        result = run_scrub_and_collect(needs_scrub_path, preset)
        out_path = OUTPUT_DIR / f"expected_{preset}.json"
        out_path.write_text(json.dumps(result, indent=2, sort_keys=True) + "\n", encoding="utf-8")
        print(f"  -> {out_path.name}")

    # Step 3: Run project scoping tests
    print("Running project scope test (select proj-alpha only)...")
    scope_result = run_scope_test(needs_scrub_path, ["proj-alpha"])
    scope_path = OUTPUT_DIR / "expected_scoped.json"
    scope_path.write_text(json.dumps(scope_result, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    print(f"  -> {scope_path.name}")

    # Step 4: Write expected DDL files
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

-- Optimize FTS index
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
    m.ack_required,
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
    m.project_id,
    m.thread_id,
    m.created_ts,
    json_extract(value, '$.type') AS attachment_type,
    json_extract(value, '$.media_type') AS media_type,
    json_extract(value, '$.path') AS path,
    CAST(json_extract(value, '$.bytes') AS INTEGER) AS size_bytes
FROM messages m,
     json_each(m.attachments)
WHERE m.attachments != '[]';

CREATE INDEX idx_attach_by_msg ON attachments_by_message_mv(message_id);
CREATE INDEX idx_attach_by_type ON attachments_by_message_mv(attachment_type, created_ts DESC);
CREATE INDEX idx_attach_by_project ON attachments_by_message_mv(project_id, created_ts DESC);

-- fts_search_overview_mv: Pre-computed search result snippets (requires FTS5)
DROP TABLE IF EXISTS fts_search_overview_mv;
CREATE TABLE fts_search_overview_mv AS
SELECT
    m.rowid,
    m.id,
    m.subject,
    m.created_ts,
    m.importance,
    a.name AS sender_name,
    SUBSTR(m.body_md, 1, 200) AS snippet
FROM messages m
JOIN agents a ON m.sender_id = a.id
ORDER BY m.created_ts DESC;

CREATE INDEX idx_fts_overview_rowid ON fts_search_overview_mv(rowid);
CREATE INDEX idx_fts_overview_created ON fts_search_overview_mv(created_ts DESC);
"""
    (OUTPUT_DIR / "expected_views_ddl.sql").write_text(views_ddl, encoding="utf-8")

    # Step 5: Record source DB hashes
    hashes = {
        "minimal": _sha256_file(minimal_path),
        "with_attachments": _sha256_file(attachments_path),
        "needs_scrub": _sha256_file(needs_scrub_path),
    }
    (OUTPUT_DIR / "source_db_hashes.json").write_text(
        json.dumps(hashes, indent=2, sort_keys=True) + "\n", encoding="utf-8"
    )

    print("\nAll share/export fixtures generated successfully.")
    print(f"Output: {OUTPUT_DIR}")


if __name__ == "__main__":
    main()
