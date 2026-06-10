PRAGMA foreign_keys = ON;
CREATE TABLE projects (
    id INTEGER PRIMARY KEY,
    slug TEXT NOT NULL UNIQUE,
    human_key TEXT NOT NULL,
    created_ts TEXT NOT NULL
);
CREATE TABLE agents (
    id INTEGER PRIMARY KEY,
    project_id INTEGER NOT NULL REFERENCES projects(id),
    name TEXT NOT NULL,
    program TEXT NOT NULL,
    model TEXT NOT NULL,
    task_description TEXT NOT NULL DEFAULT '',
    created_ts TEXT NOT NULL
);
CREATE TABLE messages (
    id INTEGER PRIMARY KEY,
    project_id INTEGER NOT NULL REFERENCES projects(id),
    sender_id INTEGER NOT NULL REFERENCES agents(id),
    thread_id TEXT NOT NULL,
    subject TEXT NOT NULL,
    body_md TEXT NOT NULL,
    importance TEXT NOT NULL DEFAULT 'normal',
    ack_required INTEGER NOT NULL DEFAULT 0,
    created_ts TEXT NOT NULL,
    recipients_json TEXT NOT NULL DEFAULT '{}',
    attachments TEXT NOT NULL DEFAULT '[]'
);
CREATE TABLE message_recipients (
    message_id INTEGER NOT NULL REFERENCES messages(id),
    agent_id INTEGER NOT NULL REFERENCES agents(id),
    kind TEXT NOT NULL DEFAULT 'to',
    read_ts TEXT,
    ack_ts TEXT,
    PRIMARY KEY (message_id, agent_id)
);
INSERT INTO projects (id, slug, human_key, created_ts)
VALUES (1, 'legacy-python-project', '/tmp/agent-mail-legacy-text-ts', '2026-06-09T10:24:35Z');
INSERT INTO agents (id, project_id, name, program, model, task_description, created_ts)
VALUES (1, 1, 'LegacyAgent', 'python-agent-mail', 'legacy', 'legacy timestamp fixture', '2026-06-09T10:24:36Z');
INSERT INTO messages (
    id, project_id, sender_id, thread_id, subject, body_md,
    importance, ack_required, created_ts, recipients_json, attachments
)
VALUES (
    1, 1, 1, 'legacy-thread', 'Legacy TEXT timestamp', 'Fixture body',
    'normal', 1, '2026-06-09T10:24:37Z', '{"to":["LegacyAgent"]}', '[]'
);
INSERT INTO message_recipients (message_id, agent_id, kind, read_ts, ack_ts)
VALUES (1, 1, 'to', '2026-06-09T10:24:38Z', NULL);
