"""Canonical-SQLite validation of the bounded overview read plan.

Reads SQL projections and the page budget from the production Rust module.
The Python control flow is an independent model, not execution of Rust or
FrankenSQLite. Peaks below describe result buffers/maps, not engine memory.
Run: python3 scripts/verify_gh274_overview.py
"""
from contextlib import closing
from dataclasses import dataclass, field
from pathlib import Path
import hashlib
import json
import random
import re
import sqlite3
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]
SOURCE_PATH = ROOT / 'crates/mcp-agent-mail-cli/src/robot/overview.rs'
SOURCE = SOURCE_PATH.read_text()
SQL = dict(re.findall(r'const (\w+_SQL): &str = "([^"]+)";', SOURCE))
PAGE = int(re.search(r'const OVERVIEW_PAGE_ROWS: usize = (\d+);', SOURCE)[1])
PENDING = re.search(r'const RECIPIENT_FILTER: &str = "([^"]+)";', SOURCE)[1]
NOW = 18_000_000_000
THRESHOLD = NOW - 1_800_000_000


def fixture(legacy=False, ledger=True):
    db = sqlite3.connect(':memory:')
    db.executescript('''
        CREATE TABLE projects(id INTEGER PRIMARY KEY, slug TEXT NOT NULL UNIQUE);
        CREATE TABLE agents(id INTEGER PRIMARY KEY, project_id INTEGER NOT NULL);
        CREATE INDEX idx_agents_project ON agents(project_id);
        CREATE TABLE messages(id INTEGER PRIMARY KEY, project_id INTEGER NOT NULL,
            importance TEXT, ack_required INTEGER, created_ts INTEGER);
        CREATE INDEX idx_messages_project_created ON messages(project_id, created_ts);
        CREATE TABLE message_recipients(message_id INTEGER NOT NULL, agent_id INTEGER NOT NULL,
            read_ts INTEGER, ack_ts INTEGER, PRIMARY KEY(message_id, agent_id));
        CREATE INDEX idx_mr_ack_message ON message_recipients(ack_ts, message_id);
        CREATE TABLE file_reservations(id INTEGER PRIMARY KEY, project_id INTEGER NOT NULL,
            created_ts INTEGER NOT NULL, expires_ts INTEGER NOT NULL);
        CREATE INDEX idx_file_reservations_expires_ts ON file_reservations(expires_ts);
    ''')
    if legacy:
        db.execute('ALTER TABLE file_reservations ADD COLUMN released_ts INTEGER')
        db.execute('CREATE INDEX idx_fr_legacy ON file_reservations(released_ts, expires_ts, id, project_id)')
    if ledger:
        db.execute('CREATE TABLE file_reservation_releases(reservation_id INTEGER PRIMARY KEY, released_ts INTEGER)')
    return db


def release_schema(db):
    legacy = any(row[1] == 'released_ts' for row in db.execute('PRAGMA table_info(file_reservations)'))
    ledger = bool(db.execute('PRAGMA table_info(file_reservation_releases)').fetchall())
    # Differential fixtures use only integer/NULL legacy values. Native tests
    # additionally use the real DB crate's full legacy-sentinel predicate.
    predicate = '(fr.released_ts IS NULL OR fr.released_ts <= 0)' if legacy else '1 = 1'
    return predicate, ledger


def reference(db, now=NOW):
    """Independent joined/counting oracle; no paged helper is shared."""
    projects = dict(db.execute('SELECT id, slug FROM projects'))
    for (pid,) in db.execute('SELECT project_id FROM agents UNION SELECT project_id FROM messages'):
        projects.setdefault(pid, f'[unknown-project-{pid}]')
    predicate, has_ledger = release_schema(db)
    released = ({r[0] for r in db.execute('SELECT reservation_id FROM file_reservation_releases')}
                if has_ledger else set())
    reservations = {}
    for rid, pid in db.execute(f'SELECT id, project_id FROM file_reservations fr WHERE ({predicate}) AND expires_ts > ?', (now,)):
        if rid not in released:
            reservations[pid] = reservations.get(pid, 0) + 1
            projects.setdefault(pid, f'[unknown-project-{pid}]')
    counts = {pid: (unread, urgent, overdue) for pid, unread, urgent, overdue in db.execute('''
        SELECT m.project_id,
        SUM(CASE WHEN mr.read_ts IS NULL THEN 1 ELSE 0 END),
        SUM(CASE WHEN mr.read_ts IS NULL AND m.importance IN ('urgent', 'high') THEN 1 ELSE 0 END),
        SUM(CASE WHEN m.ack_required = 1 AND mr.ack_ts IS NULL AND m.created_ts < ? THEN 1 ELSE 0 END)
        FROM message_recipients mr JOIN messages m ON m.id = mr.message_id GROUP BY m.project_id
    ''', (now - 1_800_000_000,))}
    return sorted((slug, *counts.get(pid, (0, 0, 0)), reservations.get(pid, 0)) for pid, slug in projects.items())


@dataclass
class Work:
    queries: int = 0
    scan_rows: list = field(default_factory=lambda: [0] * 5)
    message_lookup_rows: int = 0
    release_lookup_rows: int = 0
    peak_query_rows: int = 0
    peak_message_keys: int = 0
    peak_release_keys: int = 0


def bounded(db, now=NOW, on_recipient_page=None):
    work = Work()
    projects = {}

    def project(pid):
        return projects.setdefault(pid, [f'[unknown-project-{pid}]', 0, 0, 0, 0])

    def query(sql, params=()):
        cursor = db.execute(sql, params)
        names = [entry[0] for entry in cursor.description]
        rows = [dict(zip(names, row)) for row in cursor.fetchall()]
        work.queries += 1
        work.peak_query_rows = max(work.peak_query_rows, len(rows))
        if len(rows) > PAGE:
            raise RuntimeError('row budget exceeded')
        return rows

    def pages(select, predicate, key, slot):
        after = None
        while True:
            continuation = '' if after is None else f' AND {key} > ?'
            params = () if after is None else (after,)
            rows = query(f'{select} WHERE ({predicate}){continuation} ORDER BY {key} LIMIT {PAGE}', params)
            work.scan_rows[slot] += len(rows)
            for row in rows:
                rid = row['id']
                if not isinstance(rid, int) or (after is not None and rid <= after):
                    raise RuntimeError('non-progressing cursor')
                after = rid
            if rows:
                yield rows
            if len(rows) < PAGE:
                return

    def lookup(select, key, ids, leading=()):
        if not ids:
            return []
        if len(ids) > PAGE:
            raise RuntimeError('key budget exceeded')
        marks = ','.join('?' for _ in ids)
        return query(f'{select} WHERE {key} IN ({marks}) LIMIT {PAGE}', (*leading, *ids))

    def reservation_pages(predicate):
        expiry, last_id = now, None
        while True:
            if last_id is None:
                condition, order, params = 'fr.expires_ts > ?', 'fr.expires_ts, fr.id', (expiry,)
            else:
                condition, order, params = 'fr.expires_ts = ? AND fr.id > ?', 'fr.id', (expiry, last_id)
            rows = query(f'{SQL["RESERVATIONS_SQL"]} WHERE ({predicate}) AND {condition} ORDER BY {order} LIMIT {PAGE}', params)
            work.scan_rows[4] += len(rows)
            previous = None if last_id is None else (expiry, last_id)
            for row in rows:
                nxt = row['expires_ts'], row['id']
                valid = nxt[0] > expiry if last_id is None else nxt[0] == expiry
                if not valid or (previous is not None and nxt <= previous):
                    raise RuntimeError('non-progressing reservation cursor')
                previous = nxt
            if rows:
                yield rows
            if len(rows) < PAGE:
                if last_id is None:
                    return
                last_id = None
            else:
                expiry, last_id = previous

    db.execute('SAVEPOINT robot_overview_read')
    try:
        for rows in pages(SQL['PROJECTS_SQL'], '1 = 1', 'id', 0):
            for row in rows:
                project(row['id'])[0] = row['slug']
        for name, slot in [('AGENTS_SQL', 1), ('MESSAGE_INVENTORY_SQL', 2)]:
            for rows in pages(SQL[name], '1 = 1', 'id', slot):
                for row in rows:
                    project(row['project_id'])
        for rows in pages(SQL['RECIPIENTS_SQL'], PENDING, '_rowid_', 3):
            if on_recipient_page:
                on_recipient_page()
            ids = sorted({row['message_id'] for row in rows if isinstance(row['message_id'], int)})
            metadata = lookup(SQL['MESSAGES_SQL'], 'id', ids, (now - 1_800_000_000,))
            work.message_lookup_rows += len(metadata)
            messages = {row['id']: row for row in metadata}
            work.peak_message_keys = max(work.peak_message_keys, len(messages))
            for row in rows:
                message = messages.get(row['message_id'])
                if message is not None:
                    counts = project(message['project_id'])
                    counts[1] += row['unread']
                    counts[2] += row['unread'] * message['urgent']
                    counts[3] += row['unacked'] * message['overdue']
        predicate, has_ledger = release_schema(db)
        for rows in reservation_pages(predicate):
            ids = [row['id'] for row in rows]
            releases = lookup(SQL['RELEASE_LOOKUP_SQL'], 'reservation_id', ids) if has_ledger else []
            work.release_lookup_rows += len(releases)
            released = {row['reservation_id'] for row in releases}
            work.peak_release_keys = max(work.peak_release_keys, len(released))
            for row in rows:
                if row['id'] not in released:
                    project(row['project_id'])[4] += 1
        result = sorted(tuple(row) for row in projects.values())
    except Exception:
        # Preserve the original query failure, matching build_at's error priority.
        try:
            db.execute('RELEASE robot_overview_read')
        except sqlite3.Error:
            pass
        raise
    db.execute('RELEASE robot_overview_read')
    return result, work


def seed_scale(db, projects, per_project, pending=True):
    db.executemany('INSERT INTO projects VALUES (?, ?)', [(p, f'p{p}') for p in range(projects)])
    db.executemany('INSERT INTO agents VALUES (?, ?)', [(p, p) for p in range(projects)])
    db.executemany('INSERT INTO messages VALUES (?, ?, ?, ?, ?)',
                   [(m, m // per_project, 'high', 1, THRESHOLD - 1) for m in range(projects * per_project)])
    recipients = [(0, None, None), (1, 0, None), (2, 0, 0)] if pending else [(0, 0, 0), (1, 0, 0), (2, 0, 0)]
    db.executemany('INSERT INTO message_recipients VALUES (?, ?, ?, ?)',
                   [(m, r, read, ack) for m in range(projects * per_project) for r, read, ack in recipients])
    db.executemany('INSERT INTO file_reservations VALUES (?, ?, ?, ?)',
                   [(m, m // per_project, 0, NOW - 1) for m in range(projects * per_project)])
    db.commit()


class DifferentialTests(unittest.TestCase):
    def assert_bounded(self, work):
        self.assertLessEqual(work.peak_query_rows, PAGE)
        self.assertLessEqual(work.peak_message_keys, PAGE)
        self.assertLessEqual(work.peak_release_keys, PAGE)

    def test_empty(self):
        with closing(fixture()) as db:
            self.assertEqual(reference(db), bounded(db)[0])

    def test_randomized_exact_counts_across_release_schemas(self):
        for seed in range(100):
            legacy, ledger = bool(seed & 1), bool(seed & 2)
            with self.subTest(seed=seed), closing(fixture(legacy, ledger)) as db:
                rng = random.Random(seed)
                db.executemany('INSERT INTO projects VALUES (?, ?)', [(p, f'p{p}') for p in range(1, 8)])
                db.executemany('INSERT INTO agents VALUES (?, ?)', [(i, rng.randrange(-2, 13)) for i in range(20)])
                db.executemany('INSERT INTO messages VALUES (?, ?, ?, ?, ?)', [
                    (i, rng.randrange(-2, 13), rng.choice(['urgent', 'high', 'normal', 'URGENT', None]),
                     rng.choice([0, 1, 2, None]), rng.choice([None, THRESHOLD-1, THRESHOLD, THRESHOLD+1])) for i in range(100)])
                db.executemany('INSERT INTO message_recipients VALUES (?, ?, ?, ?)', [
                    (rng.randrange(120), i, rng.choice([None, 0, NOW]), rng.choice([None, 0, NOW])) for i in range(900)])
                db.executemany('INSERT INTO file_reservations (id,project_id,created_ts,expires_ts) VALUES (?, ?, ?, ?)', [
                    (i, rng.randrange(-2, 20), 0, rng.choice([NOW-1, NOW, NOW+1, NOW+2])) for i in range(1000)])
                if legacy:
                    db.execute('UPDATE file_reservations SET released_ts = CASE WHEN id % 7 = 0 THEN ? ELSE 0 END', (NOW,))
                if ledger:
                    db.executemany('INSERT INTO file_reservation_releases VALUES (?, ?)', [
                        (i, rng.choice([None, 0, NOW])) for i in rng.sample(range(1000), 300)])
                actual, work = bounded(db)
                self.assertEqual(reference(db), actual)
                self.assert_bounded(work)

    def test_large_fanout_preserves_recipient_multiplicity(self):
        with closing(fixture()) as db:
            db.execute("INSERT INTO projects VALUES (1, 'one')")
            db.execute("INSERT INTO messages VALUES (1, 1, 'high', 1, 0)")
            count = PAGE * 5 + 13
            db.executemany('INSERT INTO message_recipients VALUES (1, ?, NULL, NULL)', [(i,) for i in range(count)])
            rows, work = bounded(db)
            self.assertEqual(rows, [('one', count, count, count, 0)])
            self.assertEqual(work.peak_message_keys, 1)
            self.assert_bounded(work)

    def test_sparse_signed_rowids_and_maximum_keys(self):
        with closing(fixture()) as db:
            db.execute("INSERT INTO projects VALUES (1, 'one')")
            minimum, maximum = -(2**63), 2**63 - 1
            db.executemany("INSERT INTO messages VALUES (?, 1, 'high', 1, 0)", [(minimum,), (maximum,)])
            keys = [minimum, maximum, *range(-PAGE, 0)]
            db.executemany('INSERT INTO message_recipients (_rowid_,message_id,agent_id,read_ts,ack_ts) VALUES (?, ?, ?, NULL, NULL)',
                           [(key, minimum if i % 2 else maximum, i) for i, key in enumerate(keys)])
            rows, work = bounded(db)
            self.assertEqual(rows[0][1:4], (len(keys),) * 3)
            self.assert_bounded(work)

    def test_release_history_is_not_materialized(self):
        with closing(fixture()) as db:
            db.execute("INSERT INTO projects VALUES (1, 'one')")
            db.executemany('INSERT INTO file_reservation_releases VALUES (?, NULL)', [(i,) for i in range(20000)])
            db.executemany('INSERT INTO file_reservations VALUES (?, 1, 0, ?)', [(1, NOW+1), (20001, NOW+1)])
            rows, work = bounded(db)
            self.assertEqual(rows, [('one', 0, 0, 0, 1)])
            self.assertEqual(work.release_lookup_rows, 1)
            self.assert_bounded(work)

    def test_equal_expiry_pages_and_exact_maximum_boundary(self):
        with closing(fixture()) as db:
            maximum = 2**63 - 1
            db.execute("INSERT INTO projects VALUES (1, 'one')")
            db.executemany('INSERT INTO file_reservations VALUES (?, 1, 0, ?)',
                           [(i, maximum) for i in range(PAGE * 3 - 1)] + [(maximum, maximum)])
            rows, work = bounded(db, maximum - 1)
            self.assertEqual(rows[0][4], PAGE * 3)
            self.assertEqual(work.scan_rows[4], PAGE * 3)
            self.assert_bounded(work)

    def test_time_boundaries_and_mutations_without_max_timestamp_advance(self):
        with closing(fixture()) as db:
            db.execute("INSERT INTO projects VALUES (1, 'one')")
            db.execute("INSERT INTO messages VALUES (1, 1, 'high', 1, ?)", (THRESHOLD,))
            db.executemany('INSERT INTO message_recipients VALUES (1, ?, ?, ?)', [(1, None, None), (2, 999, 999)])
            db.execute('INSERT INTO file_reservations VALUES (1, 1, 0, ?)', (NOW+1,))
            self.assertEqual(bounded(db)[0], [('one', 1, 1, 0, 1)])
            self.assertEqual(bounded(db, NOW+1)[0], [('one', 1, 1, 1, 0)])
            db.execute('UPDATE message_recipients SET read_ts=1, ack_ts=1 WHERE agent_id=1')
            db.execute('INSERT INTO file_reservation_releases VALUES (1, NULL)')
            self.assertEqual(bounded(db)[0], [('one', 0, 0, 0, 0)])

    def test_snapshot_stays_consistent_across_concurrent_wal_commit_between_pages(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / 'snapshot.sqlite3'
            with closing(fixture()) as source, closing(sqlite3.connect(path)) as writer:
                seed_scale(source, 2, PAGE)
                source.backup(writer)
                writer.execute('PRAGMA journal_mode=WAL')
                expected = reference(writer)
                changed = False
                def mutate():
                    nonlocal changed
                    if not changed:
                        writer.execute('UPDATE message_recipients SET read_ts=1, ack_ts=1')
                        writer.execute("INSERT INTO projects VALUES (999, 'later')")
                        writer.commit()
                        changed = True
                with closing(sqlite3.connect(path.as_uri() + '?mode=ro', uri=True)) as reader:
                    reader.execute('PRAGMA query_only=ON')
                    self.assertEqual(bounded(reader, on_recipient_page=mutate)[0], expected)
                    self.assertTrue(changed)
                    self.assertEqual(bounded(reader)[0], reference(writer))
                    self.assertNotEqual(bounded(reader)[0], expected)

    def test_snapshot_preserves_outer_transaction_and_cleans_up_lookup_error(self):
        with closing(fixture()) as db:
            db.execute('BEGIN')
            db.execute("INSERT INTO projects VALUES (1, 'uncommitted')")
            self.assertEqual(len(bounded(db)[0]), 1)
            db.rollback()
            self.assertEqual(bounded(db)[0], [])
            seed_scale(db, 1, PAGE + 1)
            db.execute('DROP TABLE file_reservation_releases')
            db.execute('CREATE TABLE file_reservation_releases (wrong_column INTEGER)')
            db.execute('UPDATE file_reservations SET expires_ts=?', (NOW+1,))
            with self.assertRaises(sqlite3.OperationalError):
                bounded(db)
            with self.assertRaises(sqlite3.OperationalError):
                db.execute('RELEASE robot_overview_read')
            db.execute('DROP TABLE file_reservation_releases')
            db.execute('CREATE TABLE file_reservation_releases (reservation_id INTEGER PRIMARY KEY, released_ts INTEGER)')
            self.assertEqual(bounded(db)[0], reference(db))

    def test_scale_evidence_and_completed_history(self):
        evidence = []
        for projects, per_project, pending in [(1, 10, True), (10, 100, True), (50, 480, True), (50, 480, False)]:
            with closing(fixture()) as db:
                seed_scale(db, projects, per_project, pending)
                rows, work = bounded(db)
                self.assertEqual(rows, reference(db))
                self.assert_bounded(work)
                self.assertEqual(work.scan_rows[3], projects * per_project * (2 if pending else 0))
                if not pending:
                    self.assertEqual(work.message_lookup_rows, 0)
                evidence.append(dict(projects=projects, messages=projects*per_project,
                                     recipients=projects*per_project*3, pending=pending, **vars(work)))
        print(json.dumps({'engine': 'canonical SQLite', 'version': sqlite3.sqlite_version,
                          'source_sha256': hashlib.sha256(SOURCE.encode()).hexdigest(),
                          'metric': 'data query/result buffer counts; NOT native memory or latency',
                          'page_rows': PAGE, 'evidence': evidence}, sort_keys=True))

    def test_dispatch_and_production_query_contract(self):
        wrapper = (ROOT / 'crates/mcp-agent-mail-cli/src/robot.rs').read_text()
        self.assertIn('overview::build(&conn)?', wrapper)
        self.assertIn('commands::handle_robot(args)', wrapper)
        self.assertNotIn('build_overview_with_snapshot_cache', wrapper)
        self.assertEqual(set(SQL), {'PROJECTS_SQL', 'AGENTS_SQL', 'MESSAGE_INVENTORY_SQL',
                                   'MESSAGES_SQL', 'RECIPIENTS_SQL', 'RESERVATIONS_SQL', 'RELEASE_LOOKUP_SQL'})
        for sql in SQL.values():
            self.assertNotRegex(sql.upper(), r'\b(JOIN|GROUP BY|DISTINCT|OFFSET)\b')
        self.assertIn('SAVEPOINT robot_overview_read', SOURCE)
        self.assertIn('RELEASE robot_overview_read', SOURCE)
        self.assertIn('scan_reservation_pages(conn', SOURCE)


if __name__ == '__main__':
    unittest.main(verbosity=2)
