-- FTS5 virtual table for share/export search
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
