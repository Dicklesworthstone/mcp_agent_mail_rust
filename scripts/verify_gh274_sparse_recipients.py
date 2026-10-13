"""Canonical SQLite diagnostics for the indexed overview recipient phase.

SQL projections/predicates come from production Rust. Control flow is an
independent Python implementation: this does NOT execute Rust/FrankenSQLite
or measure CLI startup. Run with Python's standard library from a checkout.
"""
from collections import defaultdict
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
PARENT = ROOT / 'crates/mcp-agent-mail-cli/src/robot/overview.rs'
MODULE = PARENT.with_suffix('') / 'sparse_recipients.rs'
SOURCE = PARENT.read_text()
SPARSE = MODULE.read_text()
CONSTANTS = dict(re.findall(r'const (\w+): &str = "([^"]+)";', SOURCE + '\n' + SPARSE))
PAGE = int(re.search(r'const OVERVIEW_PAGE_ROWS: usize = (\d+);', SOURCE)[1])
NOW = 18_000_000_000
AGE = 1_800_000_000


def fixture(path=':memory:', indexed=True):
    db = sqlite3.connect(path, isolation_level=None)
    db.executescript('''
        CREATE TABLE messages(id INTEGER PRIMARY KEY, project_id INTEGER NOT NULL,
            importance TEXT, ack_required INTEGER, created_ts INTEGER);
        CREATE TABLE message_recipients(message_id INTEGER NOT NULL, agent_id INTEGER NOT NULL,
            read_ts INTEGER, ack_ts INTEGER, PRIMARY KEY(message_id, agent_id));
    ''')
    if indexed:
        db.execute('CREATE INDEX ack_messages ON messages(ack_required, id)')
        db.execute('CREATE INDEX ack_recipients ON message_recipients(ack_ts, message_id)')
    return db


def quoted(name):
    return '"' + name.replace('"', '""') + '"'


def index_prefix(db, table, prefix):
    for item in db.execute(f'PRAGMA index_list({quoted(table)})'):
        if item[4] != 0:
            continue
        columns = dict((row[0], row[2]) for row in db.execute(f'PRAGMA index_info({quoted(item[1])})'))
        if all(columns.get(position) == name for position, name in enumerate(prefix)):
            return item[1]
    return None


@dataclass
class Work:
    queries: int = 0
    recipient_rows: int = 0
    message_lookup_rows: int = 0
    ack_message_rows: int = 0
    ack_count_rows: int = 0
    returned_rows: int = 0
    peak_query_rows: int = 0
    peak_keys: int = 0
    recipient_sample_rows: int = 0
    ack_seek_rows: int = 0
    sparse: bool = False


def phase(db, sparse=True, now=NOW, on_unread=None, ack_seek=True, on_ack_seek=None):
    """Model the production recipient phase, inside its enclosing read snapshot."""
    work = Work()
    counts = defaultdict(lambda: [0, 0, 0])

    def query(sql, params=()):
        cursor = db.execute(sql, params)
        names = [column[0] for column in cursor.description]
        rows = [dict(zip(names, row)) for row in cursor.fetchall()]
        work.queries += 1
        work.returned_rows += len(rows)
        work.peak_query_rows = max(work.peak_query_rows, len(rows))
        assert len(rows) <= PAGE
        return rows

    def pages(select, predicate, key, leading=()):
        after = None
        while True:
            continuation = '' if after is None else f' AND {key} > ?'
            params = leading if after is None else (*leading, after)
            rows = query(f'{select} WHERE ({predicate}){continuation} ORDER BY {key} LIMIT {PAGE}', params)
            for row in rows:
                assert isinstance(row['id'], int)
                assert after is None or row['id'] > after
                after = row['id']
            if rows:
                yield rows
            if len(rows) < PAGE:
                return

    threshold = max(-(2**63), now - AGE)
    db.execute('SAVEPOINT recipient_diagnostic')
    try:
        message_index = index_prefix(db, 'messages', ('ack_required', 'id')) if sparse else None
        recipient_index = index_prefix(db, 'message_recipients', ('ack_ts', 'message_id')) if message_index else None
        work.sparse = message_index is not None and recipient_index is not None
        if work.sparse:
            sample = query(f'{CONSTANTS["RECIPIENTS_SQL"]} WHERE ({CONSTANTS["RECIPIENT_FILTER"]}) '
                           f'ORDER BY _rowid_ LIMIT {PAGE}')
            work.recipient_sample_rows += len(sample)
            if not sample:
                return {}, work
            work.sparse = 2 * sum(row['unread'] == 0 for row in sample) >= len(sample)
        predicate = CONSTANTS['UNREAD_FILTER'] if work.sparse else CONSTANTS['RECIPIENT_FILTER']
        for rows in pages(CONSTANTS['RECIPIENTS_SQL'], predicate, '_rowid_'):
            work.recipient_rows += len(rows)
            ids = sorted({row['message_id'] for row in rows if isinstance(row['message_id'], int)})
            marks = ','.join('?' for _ in ids)
            metadata = (query(f'{CONSTANTS["MESSAGES_SQL"]} WHERE id IN ({marks}) LIMIT {PAGE}', (threshold, *ids))
                        if ids else [])
            messages = {row['id']: row for row in metadata}
            work.message_lookup_rows += len(messages)
            work.peak_keys = max(work.peak_keys, len(messages))
            for recipient in rows:
                message = messages.get(recipient['message_id'])
                if message is None:
                    continue
                value = counts[message['project_id']]
                if recipient['unread']:
                    value[0] += 1
                    value[1] += message['urgent']
                if message['overdue'] and recipient['unacked']:
                    value[2] += 1
            if on_unread:
                on_unread()
        if work.sparse:
            index = quoted(recipient_index)
            select = f'{CONSTANTS["ACK_MESSAGES_SQL"]} INDEXED BY {quoted(message_index)}'
            def next_unacked(after):
                continuation = '' if after is None else ' AND message_id > ?'
                params = () if after is None else (after,)
                rows = query(f'{CONSTANTS["ACK_PENDING_IDS_SQL"]} INDEXED BY {index} '
                             f'WHERE {CONSTANTS["ACK_PENDING_FILTER"]}{continuation} '
                             'ORDER BY message_id LIMIT 1', params)
                assert len(rows) <= 1
                work.ack_seek_rows += len(rows)
                if on_ack_seek:
                    on_ack_seek()
                if not rows:
                    return None
                value = rows[0]['message_id']
                assert isinstance(value, int) and (after is None or value > after)
                return value

            lower = next_unacked(None) if ack_seek else None
            inclusive = ack_seek
            seek_empty = True
            while not ack_seek or lower is not None:
                comparison = '>=' if inclusive else '>'
                continuation = '' if lower is None else f' AND id {comparison} ?'
                params = (threshold,) if lower is None else (threshold, lower)
                rows = query(f'{select} WHERE ({CONSTANTS["ACK_MESSAGES_FILTER"]})'
                             f'{continuation} ORDER BY id LIMIT {PAGE}', params)
                previous = None
                for row in rows:
                    value = row['id']
                    assert isinstance(value, int)
                    assert lower is None or value > lower or (inclusive and value == lower)
                    assert previous is None or value > previous
                    previous = value
                if not rows:
                    break
                work.ack_message_rows += len(rows)
                messages = {row['id']: row['project_id'] for row in rows}
                work.peak_keys = max(work.peak_keys, len(messages))
                ids = list(messages)
                marks = ','.join('?' for _ in ids)
                grouped = query(f'{CONSTANTS["ACK_COUNTS_SQL"]} INDEXED BY {index} '
                                'WHERE ack_ts IS NULL AND read_ts IS NOT NULL '
                                f'AND message_id IN ({marks}) GROUP BY message_id', ids)
                work.ack_count_rows += len(grouped)
                for row in grouped:
                    project = messages.pop(row['message_id'])
                    assert isinstance(row['pending_count'], int) and row['pending_count'] >= 0
                    counts[project][2] += row['pending_count']
                if len(rows) < PAGE:
                    break
                if ack_seek and not grouped and seek_empty:
                    seek_empty = False
                    lower = next_unacked(rows[-1]['id'])
                    inclusive = True
                else:
                    lower = rows[-1]['id']
                    inclusive = False
                    if grouped:
                        seek_empty = True
        return {key: value for key, value in counts.items() if any(value)}, work
    finally:
        db.execute('RELEASE recipient_diagnostic')


def reference(db, now=NOW):
    rows = db.execute('''
        SELECT m.project_id,
          SUM(CASE WHEN r.read_ts IS NULL THEN 1 ELSE 0 END),
          SUM(CASE WHEN r.read_ts IS NULL AND m.importance IN ('urgent','high') THEN 1 ELSE 0 END),
          SUM(CASE WHEN r.ack_ts IS NULL AND m.ack_required=1 AND m.created_ts < ? THEN 1 ELSE 0 END)
        FROM message_recipients r JOIN messages m ON m.id=r.message_id
        GROUP BY m.project_id
    ''', (max(-(2**63), now - AGE),)).fetchall()
    return {row[0]: list(row[1:]) for row in rows if any(row[1:])}


def seed_history(db, size=24000, projects=33):
    db.execute('BEGIN')
    db.executemany('INSERT INTO messages VALUES (?, ?, ?, ?, ?)',
                   ((i, i % projects + 1, 'normal', 0, 0) for i in range(1, size + 1)))
    db.executemany('INSERT INTO message_recipients VALUES (?, 1, 0, NULL)',
                   ((i,) for i in range(1, size + 1)))
    for project in range(1, projects + 1):
        mid = size + project
        db.execute("INSERT INTO messages VALUES (?, ?, 'high', 1, 0)", (mid, project))
        db.executemany('INSERT INTO message_recipients VALUES (?, ?, ?, ?)',
                       [(mid, 1, None, None), (mid, 2, 0, None), (mid, 3, 0, 0)])
    db.execute('COMMIT')


class SparseRecipientTests(unittest.TestCase):
    def assert_matches(self, db, now=NOW):
        expected = reference(db, now)
        for sparse, ack_seek in [(False, False), (True, False), (True, True)]:
            actual, work = phase(db, sparse, now, ack_seek=ack_seek)
            self.assertEqual(actual, expected)
            self.assertLessEqual(work.peak_query_rows, PAGE)
            self.assertLessEqual(work.peak_keys, PAGE)
        return work

    def test_seeded_differential_fixtures(self):
        selected_sparse = 0
        for seed in range(200):
            rng = random.Random(seed)
            db = fixture()
            self.addCleanup(db.close)
            mids = [-(2**63), 2**63 - 1, *range(-150, 150)]
            db.executemany('INSERT INTO messages VALUES (?, ?, ?, ?, ?)',
                ((mid, rng.choice([-99, 1, 2, 77]), rng.choice(['urgent', 'high', 'URGENT', 'normal', None]),
                  rng.choice([None, 0, 1, 2]), rng.choice([None, 0, NOW-AGE-1, NOW-AGE, NOW-AGE+1])) for mid in mids))
            recipients = []
            # Exercise both strategies. A fixed 50% unread population makes
            # the pending-row sample overwhelmingly unread-heavy and would
            # test only the fallback despite invoking the new entry point.
            read_values = [None, 0, 0, 1] if seed % 2 else [None, None, 0, 1]
            for mid in [*mids, 99999]:
                for agent in range(rng.randrange(5)):
                    recipients.append((mid, agent, rng.choice(read_values), rng.choice([None, None, 0, 999])))
            rng.shuffle(recipients)
            db.executemany('INSERT INTO message_recipients VALUES (?, ?, ?, ?)', recipients)
            selected_sparse += self.assert_matches(db).sparse
        self.assertGreaterEqual(selected_sparse, 80)
        self.assertLessEqual(selected_sparse, 120)

    def test_sparse_history_work_reduction(self):
        for size in [1025, 4097, 24000]:
            db = fixture()
            self.addCleanup(db.close)
            seed_history(db, size)
            old, old_work = phase(db, False)
            new, work = phase(db, True)
            self.assertEqual(new, old)
            self.assertEqual(work.recipient_rows, 33)
            self.assertEqual(work.message_lookup_rows, 33)
            self.assertEqual(work.ack_message_rows, 33)
            self.assertEqual(work.ack_count_rows, 33)
            self.assertLess(work.queries, old_work.queries)

    def test_read_ack_fanout_and_signed_cursors(self):
        db = fixture()
        self.addCleanup(db.close)
        db.execute("INSERT INTO messages VALUES (?, 77, 'urgent', 1, 0)", (-(2**63),))
        db.executemany('INSERT INTO message_recipients VALUES (?, ?, 0, NULL)',
                       ((-(2**63), i) for i in range(3*PAGE+7)))
        work = self.assert_matches(db)
        self.assertEqual(work.ack_count_rows, 1)
        self.assertEqual(work.recipient_rows, 0)
        self.assertEqual(phase(db)[0][77], [0, 0, 3*PAGE+7])
        db.execute('UPDATE message_recipients SET read_ts=NULL')
        work = self.assert_matches(db)
        self.assertEqual(work.ack_message_rows, 0)
        self.assertEqual(work.ack_count_rows, 0)

    def test_all_acknowledged_and_all_unread_skip_second_message_walk(self):
        for read, ack in [(0, 0), (None, None)]:
            db = fixture()
            self.addCleanup(db.close)
            seed_history(db, 1025)
            db.execute('UPDATE messages SET ack_required=1')
            db.execute('UPDATE message_recipients SET read_ts=?, ack_ts=?', (read, ack))
            work = self.assert_matches(db)
            self.assertEqual(work.ack_message_rows, 0)
            self.assertEqual(work.ack_count_rows, 0)

    def test_time_and_below_max_timestamp_mutations(self):
        db = fixture()
        self.addCleanup(db.close)
        db.execute("INSERT INTO messages VALUES (1, 1, 'high', 1, ?)", (NOW-AGE,))
        db.execute("INSERT INTO messages VALUES (2, 1, 'normal', 0, 0)")
        db.executemany('INSERT INTO message_recipients VALUES (?, ?, ?, ?)',
                       [(1, 1, 0, None), (2, 1, 999, 999)])
        self.assertEqual(phase(db, now=NOW)[0], {})
        self.assertEqual(phase(db, now=NOW+1)[0], {1: [0, 0, 1]})
        db.execute('UPDATE message_recipients SET ack_ts=1 WHERE message_id=1')
        self.assertEqual(phase(db, now=NOW+1)[0], {})

    def test_index_shape_fallback_and_quoted_names(self):
        for ddl in [None,
                    'CREATE INDEX wrong ON messages(id, ack_required)',
                    'CREATE INDEX wrong ON messages(ack_required,id) WHERE ack_required=1',
                    'CREATE INDEX wrong ON messages((ack_required+0),id)']:
            db = fixture(indexed=False)
            self.addCleanup(db.close)
            if ddl:
                db.execute(ddl)
            db.execute('CREATE INDEX ack_recipients ON message_recipients(ack_ts,message_id)')
            seed_history(db, 500, 1)
            work = self.assert_matches(db)
            self.assertFalse(work.sparse)
        db = fixture(indexed=False)
        self.addCleanup(db.close)
        db.execute('CREATE INDEX "ack""messages" ON messages(ack_required,id)')
        db.execute('CREATE INDEX "ack""recipients" ON message_recipients(ack_ts,message_id)')
        seed_history(db, 500, 1)
        self.assertTrue(self.assert_matches(db).sparse)

    def test_read_only_outer_transaction_and_error_cleanup(self):
        db = fixture()
        self.addCleanup(db.close)
        db.execute('BEGIN')
        db.execute("INSERT INTO messages VALUES (1,1,'high',1,0)")
        db.execute('INSERT INTO message_recipients VALUES (1,1,NULL,NULL)')
        self.assertEqual(phase(db)[0], {1: [1, 1, 1]})
        db.execute('ROLLBACK')
        self.assertEqual(phase(db)[0], {})
        db.execute('PRAGMA query_only=ON')
        self.assertEqual(phase(db)[0], {})
        db.execute('PRAGMA query_only=OFF')
        db.execute('DROP TABLE message_recipients')
        with self.assertRaises(sqlite3.OperationalError):
            phase(db)
        with self.assertRaises(sqlite3.OperationalError):
            db.execute('RELEASE recipient_diagnostic')

    def test_two_connection_wal_snapshot_covers_both_passes(self):
        with tempfile.TemporaryDirectory() as directory:
            path = str(Path(directory)/'mail.sqlite3')
            reader = fixture(path)
            writer = sqlite3.connect(path, isolation_level=None)
            try:
                reader.execute('PRAGMA journal_mode=WAL')
                seed_history(reader, 50, 1)
                expected = reference(reader)
                changed = False
                def mutate():
                    nonlocal changed
                    if not changed:
                        writer.execute('UPDATE message_recipients SET ack_ts=1,read_ts=1')
                        changed = True
                actual, _ = phase(reader, on_unread=mutate)
                self.assertTrue(changed)
                self.assertEqual(actual, expected)
                self.assertEqual(phase(reader)[0], {})
            finally:
                writer.close()
                reader.close()

    def test_settled_ack_prefix_and_non_ack_decoy_gap(self):
        for size in [PAGE * 4 + 1, PAGE * 16 + 1, 24000]:
            for decoy in [False, True]:
                db = fixture()
                self.addCleanup(db.close)
                seed_settled(db, size, decoy)
                old, old_work = phase(db, ack_seek=False)
                new, work = phase(db)
                self.assertEqual(new, old)
                self.assertEqual(new, reference(db))
                self.assertEqual(old_work.ack_message_rows, size + 33)
                self.assertEqual(work.ack_message_rows, (PAGE if decoy else 0) + 33)
                self.assertEqual(work.ack_seek_rows, 2 if decoy else 1)
                self.assertLess(work.queries, old_work.queries)

    def test_settled_suffix_ends_only_when_pending_index_is_exhausted(self):
        db = fixture()
        self.addCleanup(db.close)
        seed_settled(db, 24000, True)
        db.execute('UPDATE message_recipients SET read_ts=0,ack_ts=0 WHERE message_id>0')
        self.assertEqual(phase(db)[0], reference(db))
        self.assertEqual(phase(db)[1].ack_message_rows, PAGE)
        # Move the one harmless pending key past all required messages.
        db.execute('UPDATE message_recipients SET message_id=99999 WHERE message_id=0')
        self.assertEqual(phase(db)[0], reference(db))
        self.assertEqual(phase(db)[1].ack_message_rows, 0)

    def test_dense_read_ack_work_adds_only_one_initial_seek(self):
        db = fixture()
        self.addCleanup(db.close)
        seed_history(db, PAGE * 3 + 7)
        db.execute('UPDATE messages SET ack_required=1')
        db.execute('UPDATE message_recipients SET read_ts=0,ack_ts=NULL')
        old, old_work = phase(db, ack_seek=False)
        new, work = phase(db)
        self.assertEqual(new, old)
        self.assertEqual(new, reference(db))
        self.assertEqual(work.ack_message_rows, old_work.ack_message_rows)
        self.assertEqual(work.queries, old_work.queries + 1)
        self.assertEqual(work.ack_seek_rows, 1)

    def test_interleaved_non_ack_decoys_keep_exact_results_and_bounded_extra_queries(self):
        db = fixture()
        self.addCleanup(db.close)
        seed_interleaved_decoys(db, PAGE * 4 + 1)
        old, old_work = phase(db, ack_seek=False)
        new, work = phase(db)
        self.assertEqual(new, old)
        self.assertEqual(new, reference(db))
        # These false positives cannot be skipped. The empty streak probes
        # only once; don't add a lookahead query to every historical page.
        # Skipping the first settled key can also remove a partial page.
        self.assertLessEqual(work.queries, old_work.queries + 2)

    def test_seeded_settled_gaps_and_reopened_earlier_ids(self):
        for seed in range(200):
            rng = random.Random(seed + 500)
            db = fixture()
            self.addCleanup(db.close)
            seed_settled(db, PAGE * 5 + 7, decoy=bool(seed % 2))
            pending = rng.sample(range(1, PAGE * 5 + 8), rng.randrange(1, 9))
            db.executemany('UPDATE message_recipients SET ack_ts=NULL WHERE message_id=?',
                           ((mid,) for mid in pending))
            db.executemany('UPDATE messages SET created_ts=? WHERE id=?',
                           ((rng.choice([NOW-AGE-1, NOW-AGE, NOW-AGE+1]), mid) for mid in pending))
            self.assert_matches(db)
            db.execute('UPDATE message_recipients SET ack_ts=NULL WHERE message_id=1')
            db.execute('UPDATE messages SET created_ts=0 WHERE id=1')
            self.assert_matches(db)

    def test_exact_page_gap_and_maximum_seek_key_keep_every_obligation(self):
        for size in [PAGE, PAGE * 2 + 1]:
            db = fixture()
            self.addCleanup(db.close)
            mids = [-(2**63), *range(1, size), 2**63 - 1]
            db.executemany("INSERT INTO messages VALUES (?,77,'high',1,0)", ((mid,) for mid in mids))
            db.executemany('INSERT INTO message_recipients VALUES (?,1,0,?)',
                           ((mid, None if mid in (mids[0], mids[-1]) else 0) for mid in mids))
            self.assert_matches(db)
            self.assertEqual(phase(db)[0][77], [0, 0, 2])

    def test_wal_write_after_seek_keeps_snapshot_and_next_call_refreshes(self):
        with tempfile.TemporaryDirectory() as directory:
            path = str(Path(directory) / 'seek.sqlite3')
            reader = fixture(path)
            writer = sqlite3.connect(path, isolation_level=None)
            try:
                reader.execute('PRAGMA journal_mode=WAL')
                seed_settled(reader, PAGE * 8, True)
                expected = reference(reader)
                changed = False
                def mutate():
                    nonlocal changed
                    if not changed:
                        writer.execute('BEGIN IMMEDIATE')
                        writer.execute('UPDATE message_recipients SET read_ts=0,ack_ts=1 WHERE message_id>1')
                        writer.execute('UPDATE message_recipients SET ack_ts=NULL WHERE message_id=1')
                        writer.execute('COMMIT')
                        changed = True
                actual, _ = phase(reader, on_ack_seek=mutate)
                self.assertTrue(changed)
                self.assertEqual(actual, expected)
                fresh = phase(reader)[0]
                self.assertEqual(fresh, reference(reader))
                self.assertNotEqual(fresh, expected)
            finally:
                writer.close()
                reader.close()

    def test_seek_failure_propagates_and_releases_the_read_savepoint(self):
        db = fixture()
        self.addCleanup(db.close)
        seed_settled(db, PAGE * 4, True)
        def reject_seek(sql, params):
            if sql.startswith(CONSTANTS['ACK_PENDING_IDS_SQL']):
                raise sqlite3.OperationalError('injected seek execution failure')
        class FailingConnection:
            def execute(self, sql, params=()):
                reject_seek(sql, params)
                return db.execute(sql, params)
        with self.assertRaisesRegex(sqlite3.OperationalError, 'injected seek'):
            phase(FailingConnection())
        with self.assertRaises(sqlite3.OperationalError):
            db.execute('RELEASE recipient_diagnostic')
        self.assert_matches(db)


def seed_settled(db, size, decoy=False):
    seed_history(db, size)
    db.execute('UPDATE messages SET ack_required=1 WHERE id<=?', (size,))
    db.execute('UPDATE message_recipients SET ack_ts=0 WHERE message_id<=?', (size,))
    if decoy:
        db.execute("INSERT INTO messages VALUES (0,1,'normal',0,0)")
        db.execute('INSERT INTO message_recipients VALUES (0,1,0,NULL)')


def seed_interleaved_decoys(db, size):
    db.execute('BEGIN')
    db.executemany("INSERT INTO messages VALUES (?,1,'normal',?,0)",
                   ((i, i % 2) for i in range(1, 2 * size + 1)))
    db.executemany('INSERT INTO message_recipients VALUES (?,1,0,?)',
                   ((i, 0 if i % 2 else None) for i in range(1, 2 * size + 1)))
    db.execute('COMMIT')


def measured(db, sparse, ack_seek=True):
    steps = 0
    def progress():
        nonlocal steps
        steps += 1
        return 0
    db.set_progress_handler(progress, 1)
    try:
        rows, work = phase(db, sparse, ack_seek=ack_seek)
    finally:
        db.set_progress_handler(None, 0)
    return rows, work, steps


if __name__ == '__main__':
    suite = unittest.defaultTestLoader.loadTestsFromTestCase(SparseRecipientTests)
    result = unittest.TextTestRunner(verbosity=2).run(suite)
    if not result.wasSuccessful():
        raise SystemExit(1)
    # Both compared strategies run on the SAME connection/corpus. The
    # incumbent is the sparse algorithm on 38c9d8a8, not the older row scanner.
    # These are whole direct-recipient-phase VM counts, not Rust timings.
    for case in ['settled-ack-prefix', 'settled-ack-gap', 'dense-read-ack', 'interleaved-decoys',
                 'read-non-ack-history', 'all-unread-ack-required']:
        db = fixture()
        try:
            if case.startswith('settled-ack'):
                seed_settled(db, 24000, case == 'settled-ack-gap')
            elif case == 'interleaved-decoys':
                seed_interleaved_decoys(db, 24000)
            else:
                seed_history(db)
            if case == 'dense-read-ack':
                db.execute('UPDATE messages SET ack_required=1')
                db.execute('UPDATE message_recipients SET read_ts=0,ack_ts=NULL')
            if case == 'all-unread-ack-required':
                db.execute('UPDATE messages SET ack_required=1')
                db.execute('UPDATE message_recipients SET read_ts=NULL,ack_ts=NULL')
            old, old_work, old_vm = measured(db, True, ack_seek=False)
            new, new_work, new_vm = measured(db, True, ack_seek=True)
            assert new == old == reference(db)
            print(json.dumps({'case': case, 'sqlite_version': sqlite3.sqlite_version,
                'old_work': asdict(old_work), 'new_work': asdict(new_work),
                'old_vm_instructions': old_vm, 'new_vm_instructions': new_vm,
                'sql_projection_sha256': hashlib.sha256(json.dumps(
                    {key: CONSTANTS[key] for key in ['RECIPIENTS_SQL', 'RECIPIENT_FILTER',
                     'MESSAGES_SQL', 'UNREAD_FILTER', 'ACK_MESSAGES_SQL', 'ACK_MESSAGES_FILTER',
                     'ACK_COUNTS_SQL', 'ACK_PENDING_IDS_SQL', 'ACK_PENDING_FILTER']},
                    sort_keys=True).encode()).hexdigest(),
                'rust_sparse_sha256': hashlib.sha256(MODULE.read_bytes()).hexdigest()}, sort_keys=True))
        finally:
            db.close()
