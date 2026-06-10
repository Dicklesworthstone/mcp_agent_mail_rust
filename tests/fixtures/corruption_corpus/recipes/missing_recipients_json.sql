PRAGMA foreign_keys = ON;
CREATE TABLE projects (
    id INTEGER PRIMARY KEY,
    slug TEXT NOT NULL UNIQUE,
    human_key TEXT NOT NULL,
    created_ts INTEGER NOT NULL
);
CREATE TABLE agents (
    id INTEGER PRIMARY KEY,
    project_id INTEGER NOT NULL REFERENCES projects(id),
    name TEXT NOT NULL,
    program TEXT NOT NULL,
    model TEXT NOT NULL,
    task_description TEXT NOT NULL DEFAULT '',
    created_ts INTEGER NOT NULL
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
    created_ts INTEGER NOT NULL,
    attachments TEXT NOT NULL DEFAULT '[]'
);
CREATE TABLE message_recipients (
    message_id INTEGER NOT NULL REFERENCES messages(id),
    agent_id INTEGER NOT NULL REFERENCES agents(id),
    kind TEXT NOT NULL DEFAULT 'to',
    read_ts INTEGER,
    ack_ts INTEGER,
    PRIMARY KEY (message_id, agent_id)
);
INSERT INTO projects (id, slug, human_key, created_ts)
VALUES (1, 'fixture-project', '/tmp/agent-mail-missing-recipients-json', 1700000000000000);
INSERT INTO agents (id, project_id, name, program, model, task_description, created_ts)
VALUES (1, 1, 'FixtureAgent', 'fixture', 'fixture-model', 'missing recipients_json', 1700000000000001);
INSERT INTO messages (
    id, project_id, sender_id, thread_id, subject, body_md,
    importance, ack_required, created_ts, attachments
)
VALUES (
    1, 1, 1, 'fixture-thread', 'Missing recipients_json', 'Fixture body',
    'normal', 0, 1700000000000002, '[]'
);
INSERT INTO message_recipients (message_id, agent_id, kind, read_ts, ack_ts)
VALUES (1, 1, 'to', NULL, NULL);
