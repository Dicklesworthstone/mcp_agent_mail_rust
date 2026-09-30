"""Copy-only regressions for the br-2hpuk mailbox materialization utility."""
from __future__ import annotations

from contextlib import closing
import hashlib
import importlib.util
import json
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


def make_mailbox(path: Path, *, rows: int = 2, alter: bool = True,
                 page_size: int = 4096, body: str = 'body\x00with newline\n') -> None:
    with closing(sqlite3.connect(path)) as conn:
        conn.execute(f'PRAGMA page_size = {page_size}')
        conn.executescript(PRE_V30)
        for offset in range(rows):
            message_id = 42900 + offset
            conn.execute("INSERT INTO messages VALUES (?, 7, 11, ?, ?, ?, ?, 'high', 1, ?, ?, ?)", (
                message_id, None if offset % 2 == 0 else 'thread-1',
                None if offset % 2 == 0 else 'topic-1', 'Subject é 💌',
                body, 1790650014000000 + offset,
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

    def test_inspection_detects_short_records_even_when_integrity_is_ok(self) -> None:
        report = repair.inspect_mailbox(self.source)
        self.assertEqual(report['status'], 'requires_materialization')
        self.assertEqual(report['physical_layout']['field_counts'], {'12': 2})
        self.assertEqual(report['physical_layout']['short_record_sample_ids'], [42900, 42901])
        self.assertFalse(self.output.exists())
        self.assert_source_unchanged()

    def test_inspection_after_repair_reports_full_records(self) -> None:
        report = repair.prepare_repair(self.source, self.output)
        self.assertEqual(report['physical_layout_before']['short_records'], 2)
        self.assertEqual(report['physical_layout_after']['short_records'], 0)
        checked = repair.inspect_mailbox(self.output)
        self.assertEqual(checked['status'], 'full_records')
        self.assertEqual(checked['physical_layout']['field_counts'], {'13': 2})

    def test_large_multilevel_mailbox_with_overflow_and_small_pages(self) -> None:
        source = self.root / 'many-pages.sqlite3'
        make_mailbox(source, rows=1200, page_size=512, body='long body\n' * 300)
        original_hash = hashlib.sha256(source.read_bytes()).hexdigest()
        checked = repair.inspect_mailbox(source)
        layout = checked['physical_layout']
        self.assertEqual(layout['short_records'], 1200)
        self.assertGreater(layout['table_pages'], 100)
        self.assertGreater(layout['overflow_pages'], 1200)
        self.assertEqual(len(layout['short_record_sample_ids']), 8)
        report = repair.prepare_repair(source, self.output)
        self.assertEqual(report['physical_layout_after']['field_counts'], {'13': 1200})
        self.assertEqual(hashlib.sha256(source.read_bytes()).hexdigest(), original_hash)

    def test_maximum_page_size_and_empty_page_encoding(self) -> None:
        for rows in (0, 5):
            source = self.root / f'64k-{rows}.sqlite3'
            make_mailbox(source, rows=rows, page_size=65536, body='x' * 100000)
            checked = repair.inspect_mailbox(source)
            self.assertEqual(checked['physical_layout']['rows'], rows)
            self.assertEqual(checked['physical_layout']['short_records'], rows)
            output = self.root / f'64k-{rows}-repaired.sqlite3'
            report = repair.prepare_repair(source, output)
            self.assertEqual(report['physical_layout_after']['short_records'], 0)

    def test_signed_nine_byte_rowids_are_preserved(self) -> None:
        source = self.root / 'signed-rowids.sqlite3'
        make_mailbox(source, rows=0, alter=False)
        ids = (-(1 << 63), -1, 0, (1 << 63) - 1)
        with closing(sqlite3.connect(source)) as conn:
            for message_id in ids:
                conn.execute("INSERT INTO messages VALUES (?,7,11,NULL,NULL,'subject','body','normal',0,1,'{}','[]')", (message_id,))
            conn.execute('ALTER TABLE messages ADD COLUMN archive_metadata_json TEXT')
            conn.commit()
        report = repair.prepare_repair(source, self.output)
        self.assertEqual(report['physical_layout_before']['short_record_sample_ids'], list(ids))
        self.assertEqual(report['physical_layout_after']['field_counts'], {'13': 4})

    def test_cli_check_exit_codes_and_source_bytes(self) -> None:
        result = subprocess.run([sys.executable, str(SCRIPT), '--check', str(self.source)],
                                capture_output=True, text=True, check=False)
        self.assertEqual(result.returncode, 2)
        self.assertEqual(json.loads(result.stdout)['status'], 'requires_materialization')
        self.assertEqual(result.stderr, '')
        repair.prepare_repair(self.source, self.output)
        result = subprocess.run([sys.executable, str(SCRIPT), '--check', str(self.output)],
                                capture_output=True, text=True, check=False)
        self.assertEqual(result.returncode, 0)
        self.assertEqual(json.loads(result.stdout)['status'], 'full_records')
        self.assert_source_unchanged()

    def test_cli_check_rejects_destination_and_repair_requires_one(self) -> None:
        for arguments in ((str(self.source),), ('--check', str(self.source), str(self.output))):
            result = subprocess.run([sys.executable, str(SCRIPT), *arguments],
                                    capture_output=True, text=True, check=False)
            self.assertEqual(result.returncode, 2)
            self.assertEqual(result.stdout, '')
            self.assertIn('error:', result.stderr)
        self.assertFalse(self.output.exists())

    def test_physical_validation_failure_never_publishes_repair(self) -> None:
        original_probe = repair._physical_layout
        def residual_short_records(path: Path, root: int, count: int) -> dict:
            result = original_probe(path, root, count)
            # Fault injection: simulate an engine that elides a no-op UPDATE.
            result['short_records'] = count
            return result
        with mock.patch.object(repair, '_physical_layout', side_effect=residual_short_records):
            with self.assertRaisesRegex(repair.RepairError, 'short records remain'):
                repair.prepare_repair(self.source, self.output)
        self.assertFalse(self.output.exists())
        self.assert_source_unchanged()

    def test_physical_probe_rejects_bad_size_count_and_root(self) -> None:
        with closing(sqlite3.connect(self.source)) as conn:
            root = conn.execute("SELECT rootpage FROM sqlite_schema WHERE name='messages'").fetchone()[0]
        with self.assertRaisesRegex(repair.RepairError, 'message count differs'):
            repair._physical_layout(self.source, root, 1)
        with self.assertRaisesRegex(repair.RepairError, 'outside the database'):
            repair._physical_layout(self.source, 2**32, 2)
        for index, raw in enumerate((self.original[:-1], b'bad header',
                                    self.original[:16] + b'\x00\x00' + self.original[18:])):
            broken = self.root / f'broken-{index}.sqlite3'
            broken.write_bytes(raw)
            with self.subTest(index=index), self.assertRaises(repair.RepairError):
                repair._physical_layout(broken, root, 2)

    def test_physical_probe_rejects_btree_cycles(self) -> None:
        source = self.root / 'cycle.sqlite3'
        make_mailbox(source, rows=100, page_size=512)
        with closing(sqlite3.connect(source)) as conn:
            root = conn.execute("SELECT rootpage FROM sqlite_schema WHERE name='messages'").fetchone()[0]
        raw = bytearray(source.read_bytes())
        offset = (root - 1) * 512
        self.assertEqual(raw[offset], 5)
        pointer = int.from_bytes(raw[offset + 12:offset + 14], 'big')
        raw[offset + pointer:offset + pointer + 4] = root.to_bytes(4, 'big')
        # Only the disposable malformed fixture is written, never a mailbox.
        broken = self.root / 'cycle-broken.sqlite3'
        broken.write_bytes(raw)
        with self.assertRaisesRegex(repair.RepairError, 'cycle or shared page'):
            repair._physical_layout(broken, root, 100)

    def test_physical_probe_rejects_overflow_cycles(self) -> None:
        source = self.root / 'overflow.sqlite3'
        make_mailbox(source, rows=1, page_size=512, body='x' * 5000)
        with closing(sqlite3.connect(source)) as conn:
            root = conn.execute("SELECT rootpage FROM sqlite_schema WHERE name='messages'").fetchone()[0]
        raw = bytearray(source.read_bytes())
        offset = (root - 1) * 512
        pointer = int.from_bytes(raw[offset + 8:offset + 10], 'big')
        payload, position = repair._varint(raw, offset + pointer, offset + 512)
        _, position = repair._varint(raw, position, offset + 512)
        local = 39 + (payload - 39) % 508
        if local > 477:
            local = 39
        overflow = int.from_bytes(raw[position + local:position + local + 4], 'big')
        raw[(overflow - 1) * 512:(overflow - 1) * 512 + 4] = overflow.to_bytes(4, 'big')
        broken = self.root / 'overflow-broken.sqlite3'
        broken.write_bytes(raw)
        with self.assertRaisesRegex(repair.RepairError, 'cycle or shared page'):
            repair._physical_layout(broken, root, 1)

    def test_record_header_spanning_overflow_is_read_completely(self) -> None:
        source = self.root / 'split-header.sqlite3'
        make_mailbox(source, rows=1, page_size=512, body='x' * 400)
        with closing(sqlite3.connect(source)) as conn:
            root = conn.execute("SELECT rootpage FROM sqlite_schema WHERE name='messages'").fetchone()[0]
        raw = bytearray(source.read_bytes())
        offset = (root - 1) * 512
        cell = offset + int.from_bytes(raw[offset + 8:offset + 10], 'big')
        payload, key_start = repair._varint(raw, cell, offset + 512)
        _, start = repair._varint(raw, key_start, offset + 512)
        self.assertLessEqual(payload, 477)
        header_size, position = repair._varint(raw, start, start + payload)
        serials = []
        while position < start + header_size:
            serial, position = repair._varint(raw, position, start + header_size)
            serials.append(serial)

        # Legal, deliberately wide varints force the header into overflow
        # without allocating huge message bodies. Canonical SQLite below is
        # the independent authority that this remains a valid logical mailbox.
        def wide_varint(value: int) -> bytes:
            return bytes(128 | ((value >> shift) & 127) for shift in (21, 14, 7)) + bytes([value & 127])

        body = raw[start + header_size:start + payload]
        record = bytes([1 + 4 * len(serials)]) + b''.join(map(wide_varint, serials)) + body
        self.assertTrue(477 < len(record) < 548)
        local = 39
        overflow_page = len(raw) // 512 + 1
        new_cell = (wide_varint(len(record)) + raw[key_start:start]
                    + record[:local] + overflow_page.to_bytes(4, 'big'))
        leaf = bytearray(512)
        leaf[0] = 13
        leaf[3:5] = (1).to_bytes(2, 'big')
        leaf[5:7] = leaf[8:10] = (512 - len(new_cell)).to_bytes(2, 'big')
        leaf[-len(new_cell):] = new_cell
        raw[offset:offset + 512] = leaf
        raw.extend(b'\x00' * 4 + record[local:] + b'\x00' * (508 - len(record[local:])))
        raw[28:32] = overflow_page.to_bytes(4, 'big')
        split_source = self.root / 'legal-split-header.sqlite3'
        split_source.write_bytes(raw)
        with closing(sqlite3.connect(split_source)) as conn:
            self.assertEqual(conn.execute('PRAGMA integrity_check').fetchall(), [('ok',)])
            self.assertEqual(conn.execute('SELECT body_md FROM messages').fetchone(), ('x' * 400,))
        report = repair.prepare_repair(split_source, self.output)
        self.assertEqual(report['physical_layout_before']['field_counts'], {'12': 1})
        self.assertEqual(report['physical_layout_before']['overflow_pages'], 1)
        self.assertEqual(report['physical_layout_after']['field_counts'], {'13': 1})


if __name__ == '__main__':
    unittest.main()
