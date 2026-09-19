"""Canonical SQLite diagnostic of overview --counts reservation discovery.

Extracts projections and budgets from the Rust implementation. Python models
its control flow independently: this does NOT execute Rust or FrankenSQLite.
The comparison measures only reservation collection after project inventory,
not database opening, recipient counts, formatting, or end-to-end CLI latency.
Run: python3 scripts/verify_gh274_counts.py
"""
from contextlib import closing
from dataclasses import asdict, dataclass
from pathlib import Path
import hashlib
import json
import random
import re
import sqlite3
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]
OVERVIEW = ROOT / 'crates/mcp-agent-mail-cli/src/robot/overview.rs'
COUNTS = OVERVIEW.parent / 'overview/counts_reservations.rs'
SOURCE = OVERVIEW.read_text()
COUNT_SOURCE = COUNTS.read_text()
SQL = dict(re.findall(r'const (\w+_SQL): &str = "([^"]+)";', SOURCE + COUNT_SOURCE))
PAGE = int(re.search(r'const OVERVIEW_PAGE_ROWS: usize = (\d+);', SOURCE)[1])
BUDGETS = {key: int(value) for key, value in re.findall(
    r'const (INITIAL_PAGES|MAX_INDEX_QUERIES|MAX_ORPHAN_PAGES): usize = (\d+);', COUNT_SOURCE)}
NOW = 18_000_000_000
MIN = -(1 << 63)
MAX = (1 << 63) - 1


def quote(name):
    return '"' + name.replace('"', '""') + '"'


def fixture(legacy=False, ledger=True, project_index=True):
    db = sqlite3.connect(':memory:')
    db.executescript('''
        CREATE TABLE projects(id INTEGER PRIMARY KEY, slug TEXT NOT NULL);
        CREATE TABLE agents(id INTEGER PRIMARY KEY, project_id INTEGER NOT NULL);
        CREATE TABLE messages(id INTEGER PRIMARY KEY, project_id INTEGER NOT NULL,
            importance TEXT, ack_required INTEGER, created_ts INTEGER);
        CREATE TABLE message_recipients(message_id INTEGER, agent_id INTEGER,
            read_ts INTEGER, ack_ts INTEGER);
        CREATE TABLE file_reservations(id INTEGER PRIMARY KEY,
            project_id INTEGER NOT NULL, expires_ts INTEGER NOT NULL);
        CREATE INDEX expiry ON file_reservations(expires_ts);
    ''')
    if legacy:
        db.execute('ALTER TABLE file_reservations ADD COLUMN released_ts INTEGER')
        db.execute('CREATE INDEX legacy_expiry ON file_reservations(released_ts, expires_ts, id, project_id)')
    if ledger:
        db.execute('CREATE TABLE file_reservation_releases(reservation_id INTEGER PRIMARY KEY, released_ts INTEGER)')
    if project_index:
        db.execute('CREATE INDEX project_expiry ON file_reservations(project_id, expires_ts)')
    return db


def schema(db):
    ledger = bool(db.execute('PRAGMA table_info(file_reservation_releases)').fetchall())
    legacy = any(row[1] == 'released_ts' for row in db.execute('PRAGMA table_info(file_reservations)'))
    # Diagnostic fixtures use integer/NULL legacy sentinels. Rust uses the DB
    # crate's full predicate rather than this restricted independent model.
    predicate = '(fr.released_ts IS NULL OR fr.released_ts <= 0)' if legacy else '1 = 1'
    return predicate, ledger


def known_projects(db):
    return {row[0] for row in db.execute('''SELECT id FROM projects UNION
        SELECT project_id FROM agents UNION SELECT project_id FROM messages''')}


def oracle(db, known, now=NOW):
    """Independent straight scan; no cursor, gap, budget, or lookup helper."""
    result = set(known)
    predicate, ledger = schema(db)
    released = ({row[0] for row in db.execute('SELECT reservation_id FROM file_reservation_releases')}
                if ledger else set())
    for rid, pid in db.execute(f'''SELECT fr.id, fr.project_id FROM file_reservations fr
        WHERE ({predicate}) AND fr.expires_ts > ?''', (now,)):
        if rid not in released:
            result.add(pid)
    return result


@dataclass
class Work:
    queries: int = 0
    candidate_rows: int = 0
    release_rows: int = 0
    project_keys: int = 0
    peak_rows: int = 0
    peak_release_keys: int = 0
    indexed: bool = False
    fallback: bool = False


class Collector:
    def __init__(self, db, now=NOW, on_gap=None):
        self.db = db
        self.now = now
        self.predicate, self.ledger = schema(db)
        self.work = Work()
        self.on_gap = on_gap

    def query(self, sql, params=()):
        cursor = self.db.execute(sql, params)
        names = [column[0] for column in cursor.description]
        rows = [dict(zip(names, row)) for row in cursor.fetchall()]
        self.work.queries += 1
        self.work.peak_rows = max(self.work.peak_rows, len(rows))
        if len(rows) > PAGE:
            raise RuntimeError('row budget exceeded')
        return rows

    def pages(self, predicate, visit):
        expiry, last_id = self.now, None
        while True:
            if last_id is None:
                condition, order, params = 'fr.expires_ts > ?', 'fr.expires_ts, fr.id', (expiry,)
            else:
                condition, order, params = 'fr.expires_ts = ? AND fr.id > ?', 'fr.id', (expiry, last_id)
            rows = self.query(f'{SQL["RESERVATIONS_SQL"]} WHERE ({predicate}) AND {condition} '
                              f'ORDER BY {order} LIMIT {PAGE}', params)
            self.work.candidate_rows += len(rows)
            previous = None if last_id is None else (expiry, last_id)
            for row in rows:
                nxt = row['expires_ts'], row['id']
                if (not all(isinstance(value, int) for value in nxt)
                        or (nxt[0] != expiry if last_id is not None else nxt[0] <= expiry)
                        or (previous is not None and nxt <= previous)):
                    raise RuntimeError('non-progressing expiry cursor')
                previous = nxt
            if rows and not visit(rows):
                return False
            if len(rows) < PAGE:
                if last_id is None:
                    return True
                last_id = None
            elif previous is not None:
                expiry, last_id = previous

    def released(self, ids):
        if not self.ledger or not ids:
            return set()
        placeholders = ','.join('?' for _ in ids)
        rows = self.query(f'{SQL["RELEASE_LOOKUP_SQL"]} WHERE reservation_id IN ({placeholders}) '
                          f'LIMIT {PAGE}', ids)
        self.work.release_rows += len(rows)
        released = {row['reservation_id'] for row in rows}
        self.work.peak_release_keys = max(self.work.peak_release_keys, len(released))
        return released

    def add_orphans(self, rows, projects):
        candidates = [(row['id'], row['project_id']) for row in rows if row['project_id'] not in projects]
        releases = self.released([rid for rid, _ in candidates])
        projects.update(pid for rid, pid in candidates if rid not in releases)

    def full(self, known):
        totals = {pid: 0 for pid in known}

        def visit(rows):
            released = self.released([row['id'] for row in rows])
            for row in rows:
                if row['id'] not in released:
                    pid = row['project_id']
                    totals[pid] = totals.get(pid, 0) + 1
            return True

        self.pages(self.predicate, visit)
        return set(totals), self.work

    def index(self):
        for index in self.db.execute('PRAGMA index_list(file_reservations)'):
            if len(index) < 5 or index[4] != 0:
                continue
            columns = self.db.execute(f'PRAGMA index_info({quote(index[1])})').fetchall()
            if any(column[0] == 0 and column[2] == 'project_id' for column in columns):
                return index[1]
        return None

    def seek(self, index, projects):
        self.work.indexed = True
        started = self.work.queries
        lower = None
        for upper in [*sorted(projects), None]:
            if ((lower is None and upper == MIN) or (lower == MAX and upper is None)
                    or (lower is not None and upper is not None and lower + 1 == upper)):
                lower = upper
                continue
            after = lower
            while True:
                if self.work.queries - started >= BUDGETS['MAX_INDEX_QUERIES']:
                    return False
                predicates, params = [], []
                if after is not None:
                    predicates.append('project_id > ?')
                    params.append(after)
                if upper is not None:
                    predicates.append('project_id < ?')
                    params.append(upper)
                condition = 'WHERE ' + ' AND '.join(predicates) if predicates else ''
                rows = self.query(f'{SQL["RESERVATION_PROJECTS_SQL"]} INDEXED BY {quote(index)} '
                                  f'{condition} ORDER BY project_id LIMIT 1', params)
                self.work.project_keys += len(rows)
                if self.on_gap:
                    callback, self.on_gap = self.on_gap, None
                    callback()
                if not rows:
                    break
                if len(rows) != 1:
                    raise RuntimeError('gap returned more than one key')
                pid = rows[0]['project_id']
                if (not isinstance(pid, int) or (after is not None and pid <= after)
                        or (upper is not None and pid >= upper)):
                    raise RuntimeError('project seek left gap')
                after = pid
                pages = 0

                def visit(rows):
                    nonlocal pages
                    if any(row['project_id'] != pid for row in rows):
                        raise RuntimeError('reservation escaped its project')
                    self.add_orphans(rows, projects)
                    pages += 1
                    return (pid not in projects and pages < BUDGETS['MAX_ORPHAN_PAGES']
                            and self.work.queries - started < BUDGETS['MAX_INDEX_QUERIES'])

                complete = self.pages(f'({self.predicate}) AND fr.project_id = {pid}', visit)
                if not complete and pid not in projects:
                    return False
            lower = upper
        return True

    def counts(self, known):
        projects, pages = set(known), 0

        def initial(rows):
            nonlocal pages
            self.add_orphans(rows, projects)
            pages += 1
            return pages < BUDGETS['INITIAL_PAGES']

        if self.pages(self.predicate, initial):
            return projects, self.work
        if len(projects) < BUDGETS['MAX_INDEX_QUERIES']:
            index = self.index()
            if index is not None and self.seek(index, projects):
                return projects, self.work
        self.work.fallback = True

        def finish(rows):
            self.add_orphans(rows, projects)
            return True

        self.pages(self.predicate, finish)
        return projects, self.work


def run(db, known, counts=True, now=NOW, on_gap=None):
    db.execute('SAVEPOINT robot_overview_read')
    try:
        collector = Collector(db, now, on_gap)
        return collector.counts(known) if counts else collector.full(known)
    finally:
        db.execute('RELEASE robot_overview_read')


def seed(db, size, projects=1, released=True):
    db.executemany('INSERT INTO file_reservations(id, project_id, expires_ts) VALUES (?, ?, ?)',
                   ((i, i % projects + 1, NOW + 1) for i in range(1, size + 1)))
    if released:
        db.executemany('INSERT INTO file_reservation_releases VALUES (?, NULL)',
                       ((i,) for i in range(1, size + 1)))
    db.commit()


def measured(db, known, counts):
    instructions = 0

    def progress():
        nonlocal instructions
        instructions += 1
        return 0

    db.set_progress_handler(progress, 1)
    try:
        ids, work = run(db, known, counts)
    finally:
        db.set_progress_handler(None, 0)
    return ids, dict(asdict(work), vm_instructions=instructions)


class CountsTests(unittest.TestCase):
    def compare(self, db, known=None, now=NOW):
        known = known_projects(db) if known is None else known
        expected = oracle(db, known, now)
        old, old_work = run(db, known, False, now)
        new, new_work = run(db, known, True, now)
        self.assertEqual(old, expected)
        self.assertEqual(new, expected)
        for work in [old_work, new_work]:
            self.assertLessEqual(work.peak_rows, PAGE)
            self.assertLessEqual(work.peak_release_keys, PAGE)
        return old_work, new_work

    def test_registered_history_growth_and_small_empty_sets(self):
        for size in [0, 1, PAGE, PAGE + 1, 8 * PAGE, 24 * PAGE, 24000]:
            with closing(fixture()) as db:
                seed(db, size, 33)
                old, new = self.compare(db, set(range(1, 34)))
                self.assertEqual(new.release_rows, 0)
                self.assertEqual(new.project_keys, 0)
                self.assertEqual(old.release_rows, size)
                self.assertLessEqual(new.candidate_rows, BUDGETS['INITIAL_PAGES'] * PAGE)
                if size > 8 * PAGE:
                    self.assertLess(new.queries, old.queries)
                self.assertEqual(run(db, set(range(1, 34)), True, NOW + 1)[1].candidate_rows, 0)

    def test_all_release_schemas_and_orphan_sources(self):
        for legacy in [False, True]:
            for ledger in [False, True]:
                with closing(fixture(legacy, ledger)) as db:
                    db.execute("INSERT INTO projects VALUES (1, 'alpha')")
                    db.execute('INSERT INTO agents VALUES (1, 7)')
                    db.execute("INSERT INTO messages VALUES (1, 8, 'normal', 0, 0)")
                    db.commit()
                    seed(db, PAGE * 5, released=False)
                    db.executemany('INSERT INTO file_reservations(id, project_id, expires_ts) VALUES (?, ?, ?)',
                        [(9001, 2, NOW + 1), (9002, 3, NOW), (9003, 4, NOW + 1),
                         (9004, 5, NOW + 1), (9005, 6, NOW + 1)])
                    if ledger:
                        db.execute('INSERT INTO file_reservation_releases VALUES (9003, NULL), (9004, 0)')
                    if legacy:
                        db.execute('UPDATE file_reservations SET released_ts = 1 WHERE id = 9005')
                    self.compare(db)
                    ids = run(db, known_projects(db))[0]
                    self.assertEqual(ids, {1, 2, 7, 8} | (set() if ledger else {4, 5}) | (set() if legacy else {6}))
                    self.compare(db, now=NOW + 1)

    def test_randomized_differential_cases_cover_all_strategies(self):
        rng = random.Random(274)
        seen = set()
        for case in range(240):
            legacy, ledger = case % 2 == 0, case % 3 != 0
            with closing(fixture(legacy, ledger, case % 5 != 0)) as db:
                size = rng.randrange(0, 400) if case % 4 == 0 else rng.randrange(1300, 1800)
                project_ids = [MIN, -19, -1, 0, 1, 2, 3, 8, MAX]
                known = set(rng.sample(project_ids, rng.randrange(0, len(project_ids))))
                for rid in range(1, size + 1):
                    # Include dense active cases that reach index discovery;
                    # others mix legacy sentinels and strict expiry boundaries.
                    expiry = NOW + 1 if case % 4 == 1 else rng.choice([NOW - 1, NOW, NOW + 1, NOW + 2, MAX])
                    pid = rng.choice(project_ids)
                    db.execute('INSERT INTO file_reservations(id, project_id, expires_ts) VALUES (?, ?, ?)', (rid, pid, expiry))
                    if legacy and case % 4 != 1:
                        db.execute('UPDATE file_reservations SET released_ts = ? WHERE id = ?', (rng.choice([None, -1, 0, 1]), rid))
                    if ledger and rng.random() < 0.8:
                        db.execute('INSERT INTO file_reservation_releases VALUES (?, ?)', (rid, rng.choice([None, 0, 1])))
                _, work = self.compare(db, known)
                seen.add((work.indexed, work.fallback))
        self.assertIn((False, False), seen)
        self.assertIn((True, False), seen)
        self.assertIn((False, True), seen)

    def test_budget_exhaustion_never_truncates_orphan_inventory(self):
        with closing(fixture()) as db:
            seed(db, PAGE * 12, released=True)
            db.execute('UPDATE file_reservations SET project_id = 77')
            db.execute('INSERT INTO file_reservations VALUES (99999, 77, ?)', (NOW + 1,))
            _, work = self.compare(db, set())
            self.assertTrue(work.indexed and work.fallback)
            db.execute('INSERT INTO file_reservation_releases VALUES (99999, NULL)')
            self.compare(db, set())
            self.assertEqual(run(db, set())[0], set())
        with closing(fixture()) as db:
            seed(db, PAGE * 5, released=False)
            db.executemany('INSERT INTO file_reservations VALUES (?, ?, 0)',
                           ((10000 + i, i, ) for i in range(2, 2 * BUDGETS['MAX_INDEX_QUERIES'])))
            db.execute('INSERT INTO file_reservations VALUES (99999, 999, ?)', (NOW + 1,))
            _, work = self.compare(db, {1})
            self.assertTrue(work.indexed and work.fallback)
            self.assertEqual(run(db, {1})[0], {1, 999})

    def test_unsuitable_indexes_broad_inventory_and_signed_gaps(self):
        for ddl in [None,
                    'CREATE INDEX unsuitable ON file_reservations(project_id) WHERE project_id = 1',
                    'CREATE INDEX unsuitable ON file_reservations(expires_ts, project_id)',
                    'CREATE INDEX unsuitable ON file_reservations((project_id + 0))']:
            with closing(fixture(project_index=False)) as db:
                if ddl:
                    db.execute(ddl)
                seed(db, 5 * PAGE)
                db.execute('INSERT INTO file_reservations VALUES (99999, 77, ?)', (NOW + 1,))
                _, work = self.compare(db, {1})
                self.assertTrue(work.fallback)
                self.assertFalse(work.indexed)
        with closing(fixture(project_index=False)) as db:
            db.execute('CREATE INDEX "quoted""name" ON file_reservations(project_id DESC, expires_ts)')
            seed(db, PAGE * 5, released=False)
            db.execute('INSERT INTO file_reservations VALUES (?, ?, ?)', (MIN, MIN, MAX))
            db.execute('INSERT INTO file_reservations VALUES (?, ?, ?)', (MAX, MAX, MAX))
            self.compare(db, {-1, 0, 1})
            self.compare(db, {MIN, 0, MAX})
            _, work = self.compare(db, set(range(100)))
            self.assertTrue(work.fallback)
            self.assertFalse(work.indexed)

    def test_read_only_outer_transaction_failure_cleanup_and_retry(self):
        with closing(fixture()) as db:
            seed(db, PAGE * 5)
            db.execute('BEGIN')
            db.execute('INSERT INTO file_reservations VALUES (99999, 77, ?)', (NOW + 1,))
            self.assertEqual(run(db, {1})[0], {1, 77})
            db.rollback()
            db.execute('PRAGMA query_only = ON')
            self.assertEqual(run(db, {1})[0], {1})
            db.execute('PRAGMA query_only = OFF')
            db.execute('DROP TABLE file_reservation_releases')
            db.execute('CREATE TABLE file_reservation_releases(wrong_column INTEGER)')
            with self.assertRaises(sqlite3.OperationalError):
                run(db, set())
            with self.assertRaises(sqlite3.OperationalError):
                db.execute('RELEASE robot_overview_read')
            db.execute('DROP TABLE file_reservation_releases')
            db.execute('CREATE TABLE file_reservation_releases(reservation_id INTEGER PRIMARY KEY)')
            self.assertEqual(run(db, set())[0], {1})

    def test_wal_write_between_gap_and_candidate_read_is_snapshot_consistent(self):
        with tempfile.TemporaryDirectory() as directory, closing(fixture()) as source:
            seed(source, PAGE * 5, released=False)
            source.execute('INSERT INTO file_reservations VALUES (99999, 77, ?)', (NOW + 1,))
            source.commit()
            path = str(Path(directory) / 'mailbox.sqlite3')
            with closing(sqlite3.connect(path)) as writer:
                source.backup(writer)
                writer.execute('PRAGMA journal_mode = WAL')
                with closing(sqlite3.connect(path)) as reader:
                    reader.execute('PRAGMA query_only = ON')
                    called = []

                    def mutate():
                        called.append(True)
                        writer.execute('INSERT INTO file_reservation_releases VALUES (99999, NULL)')
                        writer.execute('INSERT INTO file_reservations VALUES (100000, 88, ?)', (NOW + 1,))
                        writer.commit()

                    before, work = run(reader, {1}, on_gap=mutate)
                    self.assertTrue(called and work.indexed)
                    self.assertEqual(before, {1, 77})
                    self.assertEqual(run(reader, {1})[0], {1, 88})

    def test_separate_connections_observe_release_insert_and_expiry(self):
        with tempfile.TemporaryDirectory() as directory, closing(fixture()) as source:
            seed(source, 5 * PAGE, released=False)
            source.execute('INSERT INTO file_reservations VALUES (99999, 77, ?)', (NOW + 1,))
            source.commit()
            path = str(Path(directory) / 'mailbox.sqlite3')
            with closing(sqlite3.connect(path)) as writer:
                source.backup(writer)
                for iteration in range(2):
                    with closing(sqlite3.connect(f'file:{path}?mode=ro', uri=True)) as reader:
                        self.assertEqual(run(reader, {1})[0], {1, 77})
                writer.execute('INSERT INTO file_reservation_releases VALUES (99999, 0)')
                writer.execute('INSERT INTO file_reservations VALUES (100000, 88, ?)', (NOW + 2,))
                writer.commit()
                with closing(sqlite3.connect(f'file:{path}?mode=ro', uri=True)) as reader:
                    self.assertEqual(run(reader, {1})[0], {1, 88})
                    self.assertEqual(run(reader, {1}, now=NOW + 2)[0], {1})


def measurements():
    results = []
    for label, size, index, known in [
        ('33_known_24000_unexpired_released', 24000, True, set(range(1, 34))),
        ('33_known_240000_unexpired_released', 240000, True, set(range(1, 34))),
        ('empty_active_set', 0, True, {1}),
        ('small_active_set', 257, True, {1}),
        ('missing_index_24000', 24000, False, set(range(1, 34))),
        ('all_unknown_released_24000', 24000, True, set()),
    ]:
        with closing(fixture(project_index=index)) as db:
            seed(db, size, 33 if '33_known' in label or 'missing' in label else 1)
            baseline, before = measured(db, known, False)
            actual, after = measured(db, known, True)
            assert actual == baseline == oracle(db, known)
            results.append({'case': label, 'full': before, 'counts_only': after})
    print(json.dumps({'sqlite_version': sqlite3.sqlite_version,
        'scope': 'reservation phase only; canonical SQLite model, NOT native Rust/FrankenSQLite',
        'source_sha256': {str(path.relative_to(ROOT)): hashlib.sha256(path.read_bytes()).hexdigest()
                          for path in [OVERVIEW, COUNTS]},
        'measurements': results}, indent=2))


if __name__ == '__main__':
    result = unittest.TextTestRunner(verbosity=2).run(unittest.defaultTestLoader.loadTestsFromTestCase(CountsTests))
    if not result.wasSuccessful():
        raise SystemExit(1)
    measurements()
