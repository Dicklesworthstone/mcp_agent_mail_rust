PRAGMA foreign_keys = ON;
CREATE TABLE parent (
    id INTEGER PRIMARY KEY,
    label TEXT NOT NULL UNIQUE
);
CREATE TABLE child (
    id INTEGER PRIMARY KEY,
    parent_id INTEGER NOT NULL REFERENCES parent(id) ON DELETE CASCADE,
    label TEXT NOT NULL
);
CREATE INDEX idx_child_parent ON child(parent_id);
INSERT INTO parent (id, label) VALUES (1, 'root');
INSERT INTO child (id, parent_id, label) VALUES (1, 1, 'leaf');
PRAGMA foreign_key_check;
