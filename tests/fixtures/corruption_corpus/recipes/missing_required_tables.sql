CREATE TABLE metadata (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
INSERT INTO metadata (key, value)
VALUES ('fixture', 'missing projects/agents/messages/message_recipients');
