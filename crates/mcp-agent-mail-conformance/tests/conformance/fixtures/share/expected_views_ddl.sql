-- message_overview_mv: Denormalized message list with sender info
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
