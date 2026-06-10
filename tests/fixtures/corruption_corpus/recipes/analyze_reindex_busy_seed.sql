PRAGMA foreign_keys = ON;
CREATE TABLE repair_probe (
    id INTEGER PRIMARY KEY,
    payload TEXT NOT NULL
);
CREATE INDEX idx_repair_probe_payload ON repair_probe(payload);
INSERT INTO repair_probe (id, payload) VALUES
    (1, 'before-partial-repair'),
    (2, 'after-cleanup-before-reindex');
ANALYZE;
