"""Copy-only regressions for the br-2hpuk mailbox materialization utility."""
from __future__ import annotations

from contextlib import closing
import hashlib
import importlib.util
from pathlib import Path
import sqlite3
import subprocess
import sys
import tempfile
import unittest
from unittest import mock

SCRIPT = Path(__file__).resolve().parents[1] / "materialize_v30_mailbox.py"
SPEC = importlib.util.spec_from_file_location("materialize_v30_mailbox", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
repair = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(repair)

PRE_V30 = """
CREATE TABLE projects (id INTEGER PRIMARY KEY, slug TEXT, human_key TEXT);
CREATE TABLE agents (id INTEGER PRIMARY KEY, project_id INTEGER, name TEXT);
CREATE TABLE messages (
 id INTEGER PRIMARY KEY AUTOINCREMENT, project_id INTEGER NOT NULL,
 sender_id INTEGER NOT NULL, thread_id TEXT, topic TEXT COLLATE NOCASE,
 subject TEXT NOT NULL, body_md TEXT NOT NULL, importance TEXT NOT NULL DEFAULT 'normal',
 ack_required INTEGER NOT NULL DEFAULT 0, created_ts INTEGER NOT NULL,
 recipients_json TEXT NOT NULL DEFAULT '{}', attachments TEXT NOT NULL DEFAULT '[]'
);
CREATE TABLE message_recipients (message_id INTEGER, agent_id INTEGER, kind TEXT,
 read_ts INTEGER, ack_ts INTEGER, PRIMARY KEY(message_id, agent_id));
CREATE INDEX idx_messages_project_created ON messages(project_id, created_ts);
CREATE TABLE _sqlmodel_migrations (id TEXT PRIMARY KEY, checksum TEXT);
INSERT INTO _sqlmodel_migrations VALUES ('v29_fixture', 'unchanged-ledger');
INSERT INTO projects VALUES (7, 'fixture', '/offline/fixture');
INSERT INTO agents VALUES (11, 7, 'RedFox'), (12, 7, 'BlueLake');
PRAGMA user_version = 29;
PRAGMA application_id = 12345;
"""


def make_mailbox(path: Path, *, rows: int = 2, alter: bool = True) -> None:
    with closing(sqlite3.connect(path)) as conn:
        conn.executescript(PRE_V30)
        for offset in range(rows):
            message_id = 42900 + offset
            conn.execute("INSERT INTO messages VALUES (?, 7, 11, ?, ?, ?, ?, 'high', 1, ?, ?, ?)", (
                message_id, None if offset % 2 == 0 else 'thread-1',
                None if offset % 2 == 0 else 'topic-1', 'Subject é 💌',
                'body\x00with newline\n', 1790650014000000 + offset,
                '{"to":["BlueLake"]}', '[]',
            ))
            conn.execute("INSERT INTO message_recipients VALUES (?,12,'to',NULL,NULL)", (message_id,))
        if alter:
            conn.execute("ALTER TABLE messages ADD COLUMN archive_metadata_json TEXT")
            conn.execute("INSERT INTO _sqlmodel_migrations VALUES ('v30_fixture','same-checksum')")
        conn.commit()


def record_widths(path: Path) -> list[int]:
    """Independent leaf-record inspection; SQL projection length cannot prove a rewrite.

    This intentionally tiny test oracle accepts only a single-leaf messages
    table, which is sufficient for the short-record fixture in this test file.
    """
    with closing(sqlite3.connect(path)) as conn:
        page = conn.execute("SELECT rootpage FROM sqlite_schema WHERE name='messages'").fetchone()[0]
    raw = path.read_bytes()
    page_size = int.from_bytes(raw[16:18], 'big') or 65536
    if page_size == 1:
        page_size = 65536
    data = raw[(page - 1) * page_size:page * page_size]
    assert data[0] == 13, "test oracle expects one table-leaf page"

    def varint(position: int) -> tuple[int, int]:
        value = 0
        for n in range(9):
            byte = data[position]
            position += 1
            value = (value << (8 if n == 8 else 7)) | (byte if n == 8 else byte & 127)
            if n == 8 or byte < 128:
                return value, position
        raise AssertionError("unreachable")

    widths = []
    count = int.from_bytes(data[3:5], 'big')
    for n in range(count):
        pointer = int.from_bytes(data[8 + n * 2:10 + n * 2], 'big')
        _, position = varint(pointer)  # payload bytes
        _, position = varint(position)  # rowid
        start = position
        size, position = varint(position)
        fields = 0
        while position < start + size:
            _, position = varint(position)
            fields += 1
        assert position == start + size
        widths.append(fields)
    return widths


class RepairTests(unittest.TestCase):
    def setUp(self) -> None:
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)
        self.source = self.root / 'source.sqlite3'
        self.output = self.root / 'repaired.sqlite3'
        make_mailbox(self.source)
        self.original = self.source.read_bytes()

    def assert_source_unchanged(self) -> None:
        self.assertEqual(self.source.read_bytes(), self.original)
        for suffix in repair.COMPANIONS:
            self.assertFalse(Path(str(self.source) + suffix).exists())

    def test_materializes_physical_records_preserving_all_logical_data(self) -> None:
        self.assertEqual(record_widths(self.source), [12, 12])
        report = repair.prepare_repair(self.source, self.output)
        self.assertEqual(record_widths(self.output), [13, 13])
        self.assertEqual(report['messages_materialized'], 2)
        self.assertFalse(report['live_mailbox_replaced'])
        self.assertEqual(report['source_sha256'], hashlib.sha256(self.original).hexdigest())
        with closing(sqlite3.connect(self.output)) as conn:
            self.assertEqual(conn.execute('PRAGMA integrity_check').fetchone(), ('ok',))
            self.assertEqual(conn.execute('SELECT id,project_id,sender_id,subject FROM messages ORDER BY id').fetchall(),
                             [(42900, 7, 11, 'Subject é 💌'), (42901, 7, 11, 'Subject é 💌')])
            self.assertEqual(conn.execute('SELECT COUNT(*) FROM message_recipients').fetchone(), (2,))
            self.assertEqual(conn.execute('PRAGMA user_version').fetchone(), (29,))
        self.assert_source_unchanged()

    def test_preserves_existing_metadata_and_mixed_old_new_records(self) -> None:
        metadata = '{"reply_to":42900,"nested":{"x":[1,null]}}'
        with closing(sqlite3.connect(self.source)) as conn:
            conn.execute('UPDATE messages SET archive_metadata_json=? WHERE id=42901', (metadata,))
            conn.commit()
        self.original = self.source.read_bytes()
        self.assertEqual(record_widths(self.source), [12, 13])
        repair.prepare_repair(self.source, self.output)
        self.assertEqual(record_widths(self.output), [13, 13])
        with closing(sqlite3.connect(self.output)) as conn:
            self.assertEqual(conn.execute('SELECT archive_metadata_json FROM messages ORDER BY id').fetchall(),
                             [(None,), (metadata,)])
        self.assert_source_unchanged()

    def test_refuses_existing_output_and_source_alias(self) -> None:
        self.output.write_bytes(b'valuable existing output')
        for destination in (self.output, self.source):
            with self.subTest(destination=destination), self.assertRaises(repair.RepairError):
                repair.prepare_repair(self.source, destination)
        self.assertEqual(self.output.read_bytes(), b'valuable existing output')
        self.assert_source_unchanged()

    def test_refuses_sidecars_including_empty_ones(self) -> None:
        for index, suffix in enumerate(repair.COMPANIONS):
            source = self.root / f'with-companion-{index}.sqlite3'
            source.write_bytes(self.original)
            sidecar = Path(str(source) + suffix)
            sidecar.write_bytes(b'')
            with self.subTest(suffix=suffix), self.assertRaises(repair.RepairError):
                repair.prepare_repair(source, self.output)
            self.assertEqual(source.read_bytes(), self.original)
            self.assertTrue(sidecar.exists())
        self.assertFalse(self.output.exists())

    def test_refuses_v29_instead_of_silently_migrating(self) -> None:
        source = self.root / 'v29.sqlite3'
        make_mailbox(source, alter=False)
        original = source.read_bytes()
        with self.assertRaisesRegex(repair.RepairError, 'refusing an upgrade'):
            repair.prepare_repair(source, self.output)
        self.assertEqual(source.read_bytes(), original)
        self.assertFalse(self.output.exists())

    def test_rolls_back_trigger_side_effects_and_never_publishes(self) -> None:
        with closing(sqlite3.connect(self.source)) as conn:
            conn.executescript("""
                CREATE TABLE audit (id INTEGER PRIMARY KEY, event TEXT);
                CREATE TRIGGER unexpected_audit AFTER UPDATE ON messages BEGIN
                    INSERT INTO audit(event) VALUES ('update');
                END;
            """)
        self.original = self.source.read_bytes()
        with self.assertRaisesRegex(repair.RepairError, 'changed logical data'):
            repair.prepare_repair(self.source, self.output)
        self.assertFalse(self.output.exists())
        self.assert_source_unchanged()

    def test_covers_without_rowid_blob_float_null_and_quoted_table_names(self) -> None:
        with closing(sqlite3.connect(self.source)) as conn:
            conn.executescript('CREATE TABLE "strange""name" (k TEXT PRIMARY KEY, v) WITHOUT ROWID;')
            conn.executemany('INSERT INTO "strange""name" VALUES (?, ?)', [
                ('null', None), ('blob', b'\x00\xff'), ('float', 1.25),
                ('integer', 2**62), ('text', '2'), ('empty', ''),
            ])
            conn.commit()
        self.original = self.source.read_bytes()
        report = repair.prepare_repair(self.source, self.output)
        self.assertEqual(report['logical_state']['tables']['strange"name']['rows'], 6)
        self.assert_source_unchanged()

    def test_empty_mailbox(self) -> None:
        source = self.root / 'empty.sqlite3'
        make_mailbox(source, rows=0)
        report = repair.prepare_repair(source, self.output)
        self.assertEqual(report['messages_materialized'], 0)
        self.assertEqual(record_widths(self.output), [])

    def test_detects_source_change_before_publication(self) -> None:
        real_hash = repair._hash_file
        def changed_hash(path: Path) -> str:
            return 'source-changed' if path == self.source else real_hash(path)
        with mock.patch.object(repair, '_hash_file', side_effect=changed_hash):
            with self.assertRaisesRegex(repair.RepairError, 'source changed'):
                repair.prepare_repair(self.source, self.output)
        self.assertFalse(self.output.exists())
        self.assert_source_unchanged()

    def test_publication_race_cannot_clobber_new_destination(self) -> None:
        real_link = repair.os.link
        def racing_link(source: Path, destination: Path) -> None:
            destination.write_bytes(b'concurrent result')
            real_link(source, destination)
        with mock.patch.object(repair.os, 'link', side_effect=racing_link):
            with self.assertRaises(FileExistsError):
                repair.prepare_repair(self.source, self.output)
        self.assertEqual(self.output.read_bytes(), b'concurrent result')
        self.assert_source_unchanged()

    def test_refuses_malformed_database(self) -> None:
        source = self.root / 'invalid.sqlite3'
        source.write_bytes(b'not a SQLite database')
        with self.assertRaises(sqlite3.DatabaseError):
            repair.prepare_repair(source, self.output)
        self.assertFalse(self.output.exists())
        self.assertEqual(source.read_bytes(), b'not a SQLite database')

    def test_cli_returns_nonzero_without_claiming_repair(self) -> None:
        result = subprocess.run([sys.executable, str(SCRIPT), str(self.source), str(self.source)],
                                capture_output=True, text=True, check=False)
        self.assertEqual(result.returncode, 1)
        self.assertEqual(result.stdout, '')
        self.assertIn('never overwritten', result.stderr)
        self.assert_source_unchanged()


if __name__ == '__main__':
    unittest.main()
