CREATE TABLE projects (
    id INTEGER PRIMARY KEY,
    slug TEXT NOT NULL UNIQUE,
    human_key TEXT NOT NULL UNIQUE,
    created_at INTEGER NOT NULL
);

CREATE TABLE agents (
    id INTEGER PRIMARY KEY,
    project_id INTEGER NOT NULL,
    name TEXT NOT NULL,
    program TEXT NOT NULL,
    model TEXT NOT NULL,
    task_description TEXT,
    inception_ts INTEGER NOT NULL,
    last_active_ts INTEGER NOT NULL,
    capabilities TEXT,
    metadata TEXT,
    FOREIGN KEY(project_id) REFERENCES projects(id)
);

CREATE TABLE file_reservations (
    id INTEGER PRIMARY KEY,
    project_id INTEGER NOT NULL,
    agent_id INTEGER NOT NULL,
    path_pattern TEXT NOT NULL,
    exclusive INTEGER NOT NULL,
    reason TEXT,
    created_ts INTEGER NOT NULL,
    expires_ts INTEGER NOT NULL,
    released_ts INTEGER,
    FOREIGN KEY(project_id) REFERENCES projects(id),
    FOREIGN KEY(agent_id) REFERENCES agents(id)
);

CREATE TABLE file_reservation_releases (
    reservation_id INTEGER PRIMARY KEY,
    released_ts INTEGER NOT NULL,
    FOREIGN KEY(reservation_id) REFERENCES file_reservations(id)
);

INSERT INTO projects (id, slug, human_key, created_at)
VALUES (1, 'reservation-regression', '/tmp/reservation-regression', 1700002000000000);

INSERT INTO agents (
    id, project_id, name, program, model, task_description, inception_ts, last_active_ts,
    capabilities, metadata
)
VALUES (
    1, 1, 'ReleaseHolder', 'codex-cli', 'gpt-5', 'fixture holder',
    1700002000000000, 1700002000000000, NULL, NULL
);

-- Drift: the release completed in archive JSON, but SQLite released_ts stayed NULL.
INSERT INTO file_reservations (
    id, project_id, agent_id, path_pattern, exclusive, reason, created_ts, expires_ts, released_ts
)
VALUES (
    201, 1, 1, 'src/stuck-null.rs', 1, 'br-bvq1x.6.4 stuck NULL released_ts fixture',
    1700002010000000, 1700005610000000, NULL
);
