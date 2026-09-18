"""Canonical-SQLite differential check, NOT a FrankenSQLite/Rust benchmark.

SQL projections are read directly from the authored Rust module. The reference
is the grouped query and project discovery from robot.rs blob f8ca6f74
(the pre-integration implementation preserved in robot_commands.rs).
"""
from pathlib import Path
import json, random, re, sqlite3, tempfile, unittest
from contextlib import closing

ROOT = Path(__file__).resolve().parents[1]
SOURCE = (ROOT / 'crates/mcp-agent-mail-cli/src/robot/overview.rs').read_text()
SQL = dict(re.findall(r'const (\w+_SQL): &str = "([^"]+)";', SOURCE))
NOW = 18_000_000_000
THRESHOLD = NOW - 1_800_000_000


def fixture():
    db = sqlite3.connect(':memory:')
    db.executescript('''
        CREATE TABLE projects(id INTEGER PRIMARY KEY, slug TEXT NOT NULL UNIQUE);
        CREATE TABLE agents(id INTEGER PRIMARY KEY, project_id INTEGER NOT NULL);
        CREATE TABLE messages(id INTEGER PRIMARY KEY, project_id INTEGER NOT NULL,
            importance TEXT, ack_required INTEGER, created_ts INTEGER);
        CREATE TABLE message_recipients(message_id INTEGER NOT NULL, agent_id INTEGER NOT NULL,
            read_ts INTEGER, ack_ts INTEGER, PRIMARY KEY(message_id, agent_id));
        CREATE TABLE file_reservations(id INTEGER PRIMARY KEY, project_id INTEGER NOT NULL,
            created_ts INTEGER NOT NULL, expires_ts INTEGER NOT NULL);
        CREATE TABLE file_reservation_releases(reservation_id INTEGER PRIMARY KEY, released_ts INTEGER);
    ''')
    return db


def reference(db):
    projects = dict(db.execute('''WITH live_projects AS (
        SELECT id AS project_id, slug FROM projects
    ), orphan_project_ids AS (
        SELECT DISTINCT m.project_id AS raw_project_id FROM messages m
        LEFT JOIN projects p ON p.id = m.project_id WHERE p.id IS NULL
        UNION SELECT DISTINCT a.project_id AS raw_project_id FROM agents a
        LEFT JOIN projects p ON p.id = a.project_id WHERE p.id IS NULL
    ) SELECT project_id, slug FROM live_projects UNION ALL
      SELECT raw_project_id, '[unknown-project-' || raw_project_id || ']' FROM orphan_project_ids'''))
    released = {r[0] for r in db.execute('SELECT reservation_id FROM file_reservation_releases')}
    reservations = {}
    for id, pid in db.execute('SELECT id, project_id FROM file_reservations WHERE expires_ts > ?', (NOW,)):
        if id not in released:
            reservations[pid] = reservations.get(pid, 0) + 1
            projects.setdefault(pid, f'[unknown-project-{pid}]')
    counts = {pid: (unread, urgent, overdue) for pid, unread, urgent, overdue in db.execute('''
        SELECT m.project_id,
        SUM(CASE WHEN mr.read_ts IS NULL THEN 1 ELSE 0 END),
        SUM(CASE WHEN mr.read_ts IS NULL AND m.importance IN ('urgent', 'high') THEN 1 ELSE 0 END),
        SUM(CASE WHEN m.ack_required = 1 AND mr.ack_ts IS NULL AND m.created_ts < ? THEN 1 ELSE 0 END)
        FROM message_recipients mr JOIN messages m ON m.id = mr.message_id GROUP BY m.project_id
    ''', (THRESHOLD,))}
    return sorted((slug, *counts.get(pid, (0, 0, 0)), reservations.get(pid, 0)) for pid, slug in projects.items())


def linear(db):
    projects = {}
    def project(pid):
        return projects.setdefault(pid, [f'[unknown-project-{pid}]', 0, 0, 0, 0])
    for pid, slug in db.execute(SQL['PROJECTS_SQL']):
        project(pid)[0] = slug
    for (pid,) in db.execute(SQL['AGENTS_SQL']):
        project(pid)
    messages = {}
    for mid, pid, urgent, overdue in db.execute(SQL['MESSAGES_SQL'], (THRESHOLD,)):
        project(pid)
        messages[mid] = pid, urgent, overdue
    lookups = 0
    for mid, unread, unacked in db.execute(SQL['RECIPIENTS_SQL']):
        lookups += 1
        if mid not in messages:
            continue
        pid, urgent, overdue = messages[mid]
        row = project(pid)
        row[1] += unread
        row[2] += unread * urgent
        row[3] += unacked * overdue
    released = {r[0] for r in db.execute('SELECT reservation_id FROM file_reservation_releases')}
    for id, pid in db.execute('SELECT id, project_id FROM file_reservations WHERE expires_ts > ?', (NOW,)):
        if id not in released:
            project(pid)[4] += 1
    return sorted(tuple(row) for row in projects.values()), lookups


# The ten-field generation query paid before the old process-local lookup.
# This diagnostic uses the ledger-only schema. It measures canonical SQLite VM
# instructions, NOT FrankenSQLite instructions or native Rust/CLI latency.
GENERATION_SQL = """SELECT
    COALESCE((SELECT COUNT(*) FROM projects), 0),
    COALESCE((SELECT COUNT(*) FROM messages), 0),
    COALESCE((SELECT MAX(created_ts) FROM messages), 0),
    COALESCE((SELECT COUNT(*) FROM message_recipients), 0),
    COALESCE((SELECT MAX(CASE WHEN COALESCE(read_ts, 0) >= COALESCE(ack_ts, 0)
        THEN COALESCE(read_ts, 0) ELSE COALESCE(ack_ts, 0) END)
        FROM message_recipients), 0),
    COALESCE((SELECT COUNT(*) FROM agents), 0),
    COALESCE((SELECT COUNT(*) FROM file_reservations), 0),
    COALESCE((SELECT MAX(CASE WHEN created_ts >= expires_ts THEN created_ts
        ELSE expires_ts END) FROM file_reservations), 0),
    COALESCE((SELECT COUNT(*) FROM file_reservation_releases), 0),
    COALESCE((SELECT MAX(released_ts) FROM file_reservation_releases), 0)"""


def previous_cold_read(db):
    db.execute(GENERATION_SQL).fetchall()
    return reference(db)


def vm_steps(db, operation):
    steps = 0
    def tick():
        nonlocal steps
        steps += 1
        return 0
    db.set_progress_handler(tick, 1)
    try:
        value = operation(db)
    finally:
        db.set_progress_handler(None, 0)
    return value, steps


def seed_scale(db, projects, per_project):
    db.executemany('INSERT INTO projects VALUES (?, ?)',
                   [(p, f'p{p}') for p in range(projects)])
    db.executemany('INSERT INTO agents VALUES (?, ?)',
                   [(p, p) for p in range(projects)])
    db.executemany('INSERT INTO messages VALUES (?, ?, ?, ?, ?)',
                   [(m, m // per_project, 'high', 1, THRESHOLD-1)
                    for m in range(projects * per_project)])
    db.executemany('INSERT INTO message_recipients VALUES (?, ?, ?, ?)',
                   [(m, r, read, ack) for m in range(projects * per_project)
                    for r, read, ack in [(0, None, None), (1, 0, None), (2, 0, 0)]])
    db.executemany('INSERT INTO file_reservations VALUES (?, ?, ?, ?)',
                   [(m, m // per_project, 0, NOW-1)
                    for m in range(projects * per_project)])
    db.commit()


class DifferentialTests(unittest.TestCase):
    def test_empty(self):
        with closing(fixture()) as db:
            self.assertEqual(reference(db), linear(db)[0])

    def test_randomized_exact_counts(self):
        for seed in range(100):
            with self.subTest(seed=seed), closing(fixture()) as db:
                rng = random.Random(seed)
                db.executemany('INSERT INTO projects VALUES (?, ?)', [(p, f'p{p}') for p in range(1, 8)])
                db.executemany('INSERT INTO agents VALUES (?, ?)', [(i, rng.randrange(-2, 13)) for i in range(20)])
                db.executemany('INSERT INTO messages VALUES (?, ?, ?, ?, ?)', [
                    (i, rng.randrange(-2, 13), rng.choice(['urgent', 'high', 'normal', 'URGENT', None]),
                     rng.choice([0, 1, 2, None]), rng.choice([None, THRESHOLD-1, THRESHOLD, THRESHOLD+1]))
                    for i in range(100)])
                db.executemany('INSERT INTO message_recipients VALUES (?, ?, ?, ?)', [
                    (rng.randrange(120), i, rng.choice([None, 0, NOW]), rng.choice([None, 0, NOW]))
                    for i in range(400)])
                db.executemany('INSERT INTO file_reservations VALUES (?, ?, ?, ?)', [
                    (i, rng.randrange(-2, 20), 0, rng.choice([NOW-1, NOW, NOW+1])) for i in range(200)])
                db.executemany('INSERT INTO file_reservation_releases VALUES (?, ?)', [
                    (i, rng.choice([None, 0, NOW])) for i in rng.sample(range(200), 70)])
                self.assertEqual(reference(db), linear(db)[0])

    def test_lookup_work_is_linear(self):
        evidence = []
        for p in [1, 10, 50]:
            with closing(fixture()) as db:
                db.executemany('INSERT INTO projects VALUES (?, ?)', [(i, f'p{i}') for i in range(p)])
                db.executemany('INSERT INTO messages VALUES (?, ?, ?, ?, ?)', [
                    (i, i // 10, 'high', 1, THRESHOLD-1) for i in range(p*10)])
                db.executemany('INSERT INTO message_recipients VALUES (?, ?, ?, ?)', [
                    (i, recipient, read, ack) for i in range(p*10)
                    for recipient, read, ack in [(0, None, None), (1, 0, None), (2, 0, 0)]])
                actual, lookups = linear(db)
                self.assertEqual(reference(db), actual)
                self.assertEqual(lookups, p * 20)
                evidence.append({'projects':p, 'messages':p*10, 'recipients':p*30,
                                 'recipient_hash_lookups':lookups})
        print(json.dumps({'canonical_sqlite_work': evidence}, sort_keys=True))

    def test_production_projections_have_no_join_or_grouping(self):
        self.assertEqual(set(SQL), {'PROJECTS_SQL', 'AGENTS_SQL', 'MESSAGES_SQL', 'RECIPIENTS_SQL'})
        for sql in SQL.values():
            self.assertNotRegex(sql.upper(), r'\b(JOIN|GROUP BY|DISTINCT)\b')

    def test_cold_query_vm_work_and_linear_scaling(self):
        evidence = []
        for projects, per_project in [(1, 10), (10, 10), (50, 10), (50, 480)]:
            with closing(fixture()) as db:
                seed_scale(db, projects, per_project)
                old, old_steps = vm_steps(db, previous_cold_read)
                (new, lookups), new_steps = vm_steps(db, linear)
                self.assertEqual(old, new)
                self.assertLess(new_steps, old_steps)
                self.assertEqual(lookups, projects * per_project * 2)
                evidence.append(dict(projects=projects, messages=projects * per_project,
                                     recipients=projects * per_project * 3,
                                     previous_cold_vm_steps=old_steps,
                                     linear_vm_steps=new_steps,
                                     recipient_hash_lookups=lookups))
        self.assertLessEqual(evidence[2]['linear_vm_steps'],
                             50 * evidence[0]['linear_vm_steps'])
        print(json.dumps({'engine': 'canonical SQLite', 'version': sqlite3.sqlite_version,
                          'metric': 'VM instructions; excludes Python hash work and CLI startup',
                          'work_comparison': evidence}, sort_keys=True))

    def test_snapshot_stays_consistent_across_a_concurrent_wal_commit(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / 'snapshot.sqlite3'
            with closing(fixture()) as source, closing(sqlite3.connect(path)) as writer:
                source.backup(writer)
                writer.execute('PRAGMA journal_mode=WAL')
                writer.execute("INSERT INTO projects VALUES (1, 'before')")
                writer.commit()
                uri = path.as_uri() + '?mode=ro'
                with closing(sqlite3.connect(uri, uri=True)) as reader:
                    reader.execute('PRAGMA query_only=ON')
                    reader.execute('SAVEPOINT robot_overview_read')
                    self.assertEqual(reader.execute(SQL['PROJECTS_SQL']).fetchall(), [(1, 'before')])
                    writer.execute("INSERT INTO projects VALUES (2, 'after')")
                    writer.commit()
                    self.assertEqual(linear(reader)[0], [('before', 0, 0, 0, 0)])
                    reader.execute('RELEASE robot_overview_read')
                    self.assertEqual(len(linear(reader)[0]), 2)

    def test_snapshot_does_not_commit_outer_transaction_and_releases_on_error(self):
        with closing(fixture()) as db:
            db.execute('BEGIN')
            db.execute("INSERT INTO projects VALUES (1, 'uncommitted')")
            db.execute('SAVEPOINT robot_overview_read')
            self.assertEqual(len(linear(db)[0]), 1)
            db.execute('RELEASE robot_overview_read')
            db.rollback()
            self.assertEqual(linear(db)[0], [])
            db.execute('SAVEPOINT robot_overview_read')
            try:
                with self.assertRaises(sqlite3.OperationalError):
                    db.execute('SELECT * FROM missing_table')
            finally:
                db.execute('RELEASE robot_overview_read')
            with self.assertRaises(sqlite3.OperationalError):
                db.execute('RELEASE robot_overview_read')

    def test_dispatch_is_wired_without_the_old_generation_lookup(self):
        wrapper = (ROOT / 'crates/mcp-agent-mail-cli/src/robot.rs').read_text()
        self.assertIn('overview::build(&conn)?', wrapper)
        self.assertIn('commands::handle_robot(args)', wrapper)
        self.assertNotIn('build_overview_with_snapshot_cache', wrapper)
        self.assertNotIn('robot_overview_snapshot_generation', wrapper)
        self.assertIn('SAVEPOINT robot_overview_read', SOURCE)
        self.assertIn('RELEASE robot_overview_read', SOURCE)

if __name__ == '__main__':
    unittest.main(verbosity=2)
