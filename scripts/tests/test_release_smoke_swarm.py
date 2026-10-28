"""Tests of the release smoke's swarm-phase judgement logic (br-kp1in.34).

These exercise helper policy and independent SQLite/archive fixtures, NOT
Agent Mail; the phase itself runs against real binaries through
tests/e2e/test_release_smoke.sh. The count-only false green also has a retained
real HTTP/mailbox control in br-kp1in.35.
"""
import importlib.util
import json
import os
from pathlib import Path
import sqlite3
import sys
import tempfile
import threading
import time
import unittest

spec = importlib.util.spec_from_file_location(
    'release_smoke',
    Path(__file__).resolve().parents[2] / 'tests' / 'e2e' / 'lib' / 'release_smoke.py',
)
assert spec is not None and spec.loader is not None
smoke = importlib.util.module_from_spec(spec)
spec.loader.exec_module(smoke)

TIMEOUT_ERROR = {'code': -32603, 'message': 'Request timed out after 30s (method=tools/call:file_reservation_paths)'}


class ClassifyCallTests(unittest.TestCase):
    def test_success_is_ok(self):
        self.assertEqual(smoke.classify_call(False, {'granted': []}), 'ok')

    def test_dispatch_deadline_is_a_timeout(self):
        self.assertEqual(smoke.classify_call(True, TIMEOUT_ERROR), 'timeout')

    def test_other_errors_are_tool_errors(self):
        busy = {'error': {'type': 'RESOURCE_BUSY', 'message': 'database is busy'}}
        self.assertEqual(smoke.classify_call(True, busy), 'tool_error')
        self.assertEqual(smoke.error_code(busy), 'RESOURCE_BUSY')


class KillWindowTests(unittest.TestCase):
    def test_call_in_flight_at_the_kill_is_excused(self):
        self.assertTrue(smoke.in_kill_window(start=95.0, end=100.5, kill_t=100.0, restarted_t=102.0))

    def test_call_started_just_after_restart_is_excused(self):
        self.assertTrue(smoke.in_kill_window(start=104.0, end=104.1, kill_t=100.0, restarted_t=102.0))

    def test_call_long_after_restart_is_not_excused(self):
        self.assertFalse(smoke.in_kill_window(start=110.0, end=110.2, kill_t=100.0, restarted_t=102.0))

    def test_call_finished_before_the_kill_is_not_excused(self):
        self.assertFalse(smoke.in_kill_window(start=90.0, end=99.0, kill_t=100.0, restarted_t=102.0))

    def test_no_kill_excuses_nothing(self):
        self.assertFalse(smoke.in_kill_window(start=1.0, end=2.0, kill_t=None, restarted_t=None))


class ToolStatsTests(unittest.TestCase):
    def records(self):
        recs = [('file_reservation_paths', float(i), float(i) + 0.1, 'ok', '') for i in range(98)]
        # Two calls hit the 30 s dispatch deadline.
        recs += [('file_reservation_paths', 200.0, 230.0, 'timeout', 'x'),
                 ('file_reservation_paths', 201.0, 231.0, 'timeout', 'x')]
        recs += [('send_message', 99.0, 100.2, 'conn_error', 'RemoteDisconnected'),   # cut by the kill
                 ('send_message', 300.0, 300.1, 'conn_error', 'URLError'),            # not excused
                 ('send_message', 301.0, 301.2, 'tool_error', 'RESOURCE_BUSY'),
                 ('send_message', 302.0, 302.1, 'ok', '')]
        return recs

    def test_timed_out_calls_count_at_full_duration(self):
        # Planted negative: dropping timed-out calls from latency would report a
        # 0.1 s p99 and hide the stall the phase exists to catch.
        s = smoke.swarm_tool_stats(self.records(), kill_t=100.0, restarted_t=102.0)
        fr = s['file_reservation_paths']
        self.assertEqual(fr['n'], 100)
        self.assertEqual(fr['timeouts'], 2)
        self.assertGreaterEqual(fr['p99_s'], 30.0)
        self.assertGreater(fr['p99_s'], smoke.SWARM_TOOL_P99_BUDGET_S)

    def test_connection_errors_outside_the_kill_window_are_failures(self):
        s = smoke.swarm_tool_stats(self.records(), kill_t=100.0, restarted_t=102.0)
        sm = s['send_message']
        self.assertEqual(sm['kill_window'], 1)
        self.assertEqual(sm['conn_errors'], 1)
        self.assertEqual(sm['tool_errors'], {'RESOURCE_BUSY': 1})
        self.assertEqual(sm['n'], 2)

    def test_without_a_kill_every_connection_error_counts(self):
        s = smoke.swarm_tool_stats(self.records(), kill_t=None, restarted_t=None)
        self.assertEqual(s['send_message']['conn_errors'], 2)
        self.assertEqual(s['send_message']['kill_window'], 0)


class DrainWindowTests(unittest.TestCase):
    def test_progress_with_queued_work_passes(self):
        self.assertEqual(smoke.drain_stalled_windows([(0, 10, 5), (10, 20, 7), (20, 30, 0)]), [])

    def test_queued_work_without_progress_is_a_stall(self):
        bad = smoke.drain_stalled_windows([(0, 10, 5), (10, 10, 9)])
        self.assertEqual(len(bad), 1)
        self.assertEqual(bad[0]['depth_at_open'], 5)

    def test_an_empty_queue_without_progress_is_idle_not_stalled(self):
        self.assertEqual(smoke.drain_stalled_windows([(0, 10, 0), (10, 10, 0)]), [])


class KnobTests(unittest.TestCase):
    def test_release_strength_can_pass(self):
        self.assertIsNone(smoke.swarm_knob_shortfall(smoke.RELEASE_MIN_SWARM_AGENTS,
                                                     smoke.RELEASE_MIN_SWARM_SECS))

    def test_fewer_agents_or_a_shorter_run_cannot_pass(self):
        self.assertIsNotNone(smoke.swarm_knob_shortfall(smoke.RELEASE_MIN_SWARM_AGENTS - 1,
                                                        smoke.RELEASE_MIN_SWARM_SECS))
        self.assertIsNotNone(smoke.swarm_knob_shortfall(smoke.RELEASE_MIN_SWARM_AGENTS,
                                                        smoke.RELEASE_MIN_SWARM_SECS - 1))

    def test_skipped_phase_records_no_checks_so_it_is_no_verdict(self):
        saved = smoke.SWARM_AGENTS
        smoke.SWARM_AGENTS = 0
        try:
            checks: list = []
            extra = smoke.phase_swarm(None, {}, checks)
        finally:
            smoke.SWARM_AGENTS = saved
        self.assertEqual(checks, [])
        self.assertEqual(smoke.verdict(checks), 'NO_VERDICT')
        self.assertIn('cannot_pass', extra)


class ArchiveParityTests(unittest.TestCase):
    def setUp(self):
        # Retain evidence; never delete even private fixture files (AGENTS.md).
        self.root = Path(tempfile.mkdtemp(prefix='am-release-archive-witness-'))
        self.arm = smoke.Arm('witness-fixture', Path(sys.executable), self.root)
        self.body = '\nsynthetic body with significant spaces \t\n\n'
        self.envelope = {
            'id': 1, 'from': 'BlueSpring', 'to': ['RedLake'], 'cc': ['GreenStone'],
            'bcc': ['WhiteOak'], 'subject': 'witness', 'created': '2023-11-14T22:13:20.123456Z',
            'thread_id': 'thread-one', 'topic': None, 'project': self.arm.proj1,
            'project_slug': 'proof', 'importance': 'normal', 'ack_required': True,
            'attachments': [], 'reply_to': 7,
        }
        conn = sqlite3.connect(self.arm.db_path)
        try:
            conn.executescript('''
                CREATE TABLE projects(id INTEGER PRIMARY KEY, slug TEXT, human_key TEXT);
                CREATE TABLE agents(id INTEGER PRIMARY KEY, project_id INTEGER, name TEXT);
                CREATE TABLE messages(id INTEGER PRIMARY KEY, project_id INTEGER, sender_id INTEGER,
                    subject TEXT, body_md TEXT, thread_id TEXT, topic TEXT, importance TEXT,
                    ack_required INTEGER, created_ts INTEGER, attachments TEXT,
                    recipients_json TEXT, archive_metadata_json TEXT);
                CREATE TABLE message_recipients(message_id INTEGER, agent_id INTEGER, kind TEXT);
                CREATE TABLE project_mailbox_deliveries(message_id INTEGER PRIMARY KEY,
                    project_id INTEGER, kind TEXT);
            ''')
            conn.execute('INSERT INTO projects VALUES(1, ?, ?)', ('proof', self.arm.proj1))
            conn.executemany('INSERT INTO agents(id, name, project_id) VALUES(?, ?, 1)',
                             [(1, 'BlueSpring'), (2, 'RedLake'), (3, 'GreenStone'), (4, 'WhiteOak')])
            roles = {key: self.envelope[key] for key in ('to', 'cc', 'bcc')}
            conn.execute('INSERT INTO messages VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?)',
                         (1, 1, 1, 'witness', self.body, 'thread-one', None, 'normal', 1,
                          1700000000123456, '[]', json.dumps(roles), '{"reply_to":7}'))
            conn.executemany('INSERT INTO message_recipients VALUES(1,?,?)',
                             [(2, 'to'), (3, 'cc'), (4, 'bcc')])
            conn.commit()
        finally:
            conn.close()
        self.path = self.root / 'storage/projects/proof/messages/2023/11/witness__1.md'
        self.path.parent.mkdir(parents=True)
        self.write_message(self.envelope, self.body)
        (self.root / 'original-message-retained.md').write_bytes(self.path.read_bytes())

    def write_message(self, envelope, body):
        self.path.write_bytes(('---json\n' + json.dumps(envelope) + '\n---\n\n' + body).encode())

    def parity(self):
        return smoke.archive_db_parity(self.root / 'storage', self.arm.db_path)

    def update_db(self, sql, parameters=()):
        conn = sqlite3.connect(self.arm.db_path)
        try:
            conn.execute(sql, parameters)
            conn.commit()
        finally:
            conn.close()

    def test_real_snapshot_and_lossless_body_pass_without_server_requests(self):
        def forbidden_health():
            self.fail('quiet convergence must not poll the server')
        self.arm.health = forbidden_health
        self.assertTrue(self.parity()[0])
        ok, detail = self.arm.wait_converged(2, 1)
        self.assertTrue(ok, detail)
        trace = json.loads((self.root / 'out/archive_convergence.jsonl').read_text().splitlines()[0])
        self.assertTrue(trace['no_mcp_requests'])
        self.assertTrue(trace['within_bound'])

    def test_same_count_modified_body_is_rejected_without_echoing_content(self):
        self.write_message(self.envelope, 'different synthetic body')
        ok, detail = self.parity()
        self.assertFalse(ok)
        self.assertEqual(detail['db_messages'], detail['archive_messages'])
        self.assertIn('body_md', detail['mismatches'][0]['fields'])
        self.assertNotIn('different synthetic body', json.dumps(detail))

    def test_same_count_unrelated_id_cannot_replace_the_expected_message(self):
        self.path.rename(self.root / 'displaced-original-retained.md')
        self.path = self.path.with_name('unrelated__99.md')
        self.write_message({**self.envelope, 'id': 99}, self.body)
        ok, detail = self.parity()
        self.assertFalse(ok)
        self.assertEqual(detail['db_messages'], detail['archive_messages'])
        self.assertEqual(detail['missing_ids'], [1])
        self.assertEqual(detail['extra_ids'], [99])

    def test_same_count_wrong_recipient_role_is_rejected(self):
        self.write_message({**self.envelope, 'to': ['BlueSpring']}, self.body)
        ok, detail = self.parity()
        self.assertFalse(ok)
        self.assertEqual(detail['db_messages'], detail['archive_messages'])
        self.assertEqual(detail['mismatches'][0]['fields'], ['to'])

    def test_cached_routing_cannot_hide_changed_delivery_roles(self):
        self.update_db("UPDATE message_recipients SET kind='cc' WHERE agent_id=2")
        ok, detail = self.parity()
        self.assertFalse(ok)
        self.assertIn('db_error', detail)

    def test_cached_routing_cannot_hide_a_missing_delivery(self):
        # Preserve every original row in another table, then expose a genuinely
        # incomplete active delivery set without deleting fixture evidence.
        self.update_db('ALTER TABLE message_recipients RENAME TO retained_message_recipients')
        self.update_db('CREATE TABLE message_recipients AS SELECT * FROM retained_message_recipients '
                       'WHERE agent_id<>2')
        self.assertFalse(self.parity()[0])

    def test_orphaned_or_foreign_recipient_identity_is_rejected(self):
        self.update_db('UPDATE message_recipients SET agent_id=999 WHERE agent_id=2')
        self.assertFalse(self.parity()[0])
        self.update_db('UPDATE message_recipients SET agent_id=2 WHERE agent_id=999')
        self.update_db("INSERT INTO projects VALUES(2, 'other', '/other')")
        self.update_db('UPDATE agents SET project_id=2 WHERE id=2')
        self.assertFalse(self.parity()[0])

    def test_explicit_project_mailbox_delivery_preserves_routing_and_scope(self):
        envelope = {**self.envelope, 'to': ['RedLake', 'project:proof']}
        roles = {role: envelope[role] for role in ('to', 'cc', 'bcc')}
        self.update_db('INSERT INTO project_mailbox_deliveries VALUES(1,1,?)', ('to',))
        self.update_db('UPDATE messages SET recipients_json=?', (json.dumps(roles),))
        self.write_message(envelope, self.body)
        self.assertTrue(self.parity()[0])
        self.update_db("INSERT INTO projects VALUES(2, 'other', '/other')")
        self.update_db('UPDATE project_mailbox_deliveries SET project_id=2')
        self.assertFalse(self.parity()[0])

    def test_legacy_empty_cache_uses_complete_delivery_rows(self):
        self.update_db("UPDATE messages SET recipients_json='{}'")
        self.assertTrue(self.parity()[0])

    def test_recipient_objects_cannot_masquerade_as_name_arrays(self):
        malformed = {**self.envelope, 'to': {'RedLake': 123}}
        self.write_message(malformed, self.body)
        self.assertFalse(self.parity()[0])
        self.write_message(self.envelope, self.body)
        roles = {role: malformed[role] for role in ('to', 'cc', 'bcc')}
        self.update_db('UPDATE messages SET recipients_json=?', (json.dumps(roles),))
        self.assertFalse(self.parity()[0])

    def test_json_metadata_types_cannot_alias_boolean_and_numeric_values(self):
        self.write_message({**self.envelope, 'ack_required': 1}, self.body)
        self.assertFalse(self.parity()[0])
        self.update_db('UPDATE messages SET attachments=?', ('[{"inline":true}]',))
        self.write_message({**self.envelope, 'attachments': [{'inline': True}]}, self.body)
        self.assertTrue(self.parity()[0])
        self.write_message({**self.envelope, 'attachments': [{'inline': 1}]}, self.body)
        ok, detail = self.parity()
        self.assertFalse(ok)
        self.assertIn('attachments', detail['mismatches'][0]['fields'])

    def test_known_reply_absence_refuses_an_added_archive_parent(self):
        self.update_db("UPDATE messages SET archive_metadata_json='{}'")
        ok, detail = self.parity()
        self.assertFalse(ok)
        self.assertEqual(detail['mismatches'][0]['fields'], ['reply_to'])
        envelope = {key: value for key, value in self.envelope.items() if key != 'reply_to'}
        self.write_message(envelope, self.body)
        self.assertTrue(self.parity()[0])

    def test_legacy_null_reply_authority_is_explicitly_unverifiable(self):
        self.update_db('UPDATE messages SET archive_metadata_json=NULL')
        ok, detail = self.parity()
        self.assertFalse(ok)
        self.assertEqual(detail['unverifiable_reply_ids'], [1])
        self.assertEqual(detail['mismatches'], [])

    def test_invalid_reply_parent_types_or_missing_thread_are_rejected(self):
        for parent in (0, 1, True, '7', None):
            with self.subTest(parent=parent):
                self.write_message({**self.envelope, 'reply_to': parent}, self.body)
                self.assertFalse(self.parity()[0])
        self.write_message({**self.envelope, 'thread_id': None}, self.body)
        self.assertFalse(self.parity()[0])
        self.write_message({**self.envelope, 'reply_to': True}, self.body)
        self.update_db('UPDATE messages SET archive_metadata_json=?', ('{"reply_to":true}',))
        self.assertFalse(self.parity()[0])

    def test_project_root_symlink_is_refused_without_following_it(self):
        projects = self.root / 'storage/projects'
        retained = self.root / 'retained-projects'
        projects.rename(retained)
        projects.symlink_to(retained, target_is_directory=True)
        ok, detail = self.parity()
        self.assertFalse(ok)
        self.assertEqual(detail['archive_error'], 'ValueError')

    @unittest.skipUnless(hasattr(os, 'mkfifo'), 'requires Unix FIFO support')
    def test_nonregular_canonical_entry_is_refused(self):
        self.path.rename(self.root / 'retained-before-fifo.md')
        os.mkfifo(self.path)
        ok, detail = self.parity()
        self.assertFalse(ok)
        self.assertEqual(detail['invalid_files'], 1)

    def test_expired_observation_deadline_cannot_pass_matching_content(self):
        ok, detail = smoke.archive_db_parity(self.root / 'storage', self.arm.db_path,
                                            deadline=time.monotonic() - 1)
        self.assertFalse(ok)
        self.assertTrue(detail['deadline_exceeded'])
        ok, detail = self.arm.wait_converged(0, 1)
        self.assertFalse(ok)
        self.assertTrue(json.loads(detail)['deadline_exceeded'])

    def test_expensive_sqlite_observation_cannot_pass_after_its_deadline(self):
        self.update_db('ALTER TABLE messages RENAME TO retained_messages')
        self.update_db('''CREATE VIEW messages AS
            WITH RECURSIVE work(n) AS (VALUES(1) UNION ALL SELECT n+1 FROM work WHERE n<100000000)
            SELECT m.* FROM retained_messages m CROSS JOIN work WHERE work.n=100000000''')
        ok, detail = smoke.archive_db_parity(self.root / 'storage', self.arm.db_path,
                                            deadline=time.monotonic() + 0.2)
        self.assertFalse(ok)
        self.assertTrue(detail['deadline_exceeded'])
        self.assertIn(detail['db_error'], ('OperationalError', 'TimeoutError'))

    def test_live_client_prevents_quiet_claim_then_stopped_client_allows_it(self):
        ready, finish = threading.Event(), threading.Event()

        def client():
            ready.set()
            finish.wait()

        thread = threading.Thread(target=client)
        thread.start()
        try:
            self.assertTrue(ready.wait(2), 'real client thread did not start')
            ok, detail = self.arm.wait_converged(2, 1, client_threads=[thread])
            self.assertFalse(ok)
            detail = json.loads(detail)
            self.assertEqual(detail['active_client_threads'], 1)
            self.assertFalse(detail['no_mcp_requests'])
            self.assertTrue(detail['convergence_unobserved'])
        finally:
            finish.set()
            thread.join(timeout=2)
        self.assertFalse(thread.is_alive())
        self.assertTrue(self.arm.wait_converged(2, 1, client_threads=[thread])[0])


def elf_image(machine: int, program_header_types: list) -> bytes:
    """A little-endian ELF64 header followed by its program headers."""
    header = bytearray(64)
    header[:6] = b'\x7fELF\x02\x01'
    header[18:20] = machine.to_bytes(2, 'little')
    header[32:40] = (64).to_bytes(8, 'little')
    header[54:56] = (56).to_bytes(2, 'little')
    header[56:58] = len(program_header_types).to_bytes(2, 'little')
    return bytes(header) + b''.join(
        p_type.to_bytes(4, 'little') + bytes(52) for p_type in program_header_types)


class ElfTargetTests(unittest.TestCase):
    def test_no_interpreter_is_static(self):
        self.assertEqual(smoke.elf_target(elf_image(0x3E, [1, 1])), 'x86_64-static')

    def test_interpreter_header_is_dynamic(self):
        self.assertEqual(smoke.elf_target(elf_image(0xB7, [6, 3, 1])), 'aarch64-dynamic')

    def test_the_running_python_is_recognised(self):
        with open(sys.executable, 'rb') as f:
            target = smoke.elf_target(f.read(1 << 16))
        self.assertIsNotNone(target)
        self.assertNotIn('unknown', target)

    def test_non_elf_and_truncated_headers_are_unknown(self):
        self.assertIsNone(smoke.elf_target(b'#!/bin/sh\n' + bytes(64)))
        self.assertIsNone(smoke.elf_target(elf_image(0x3E, [1, 1])[:100]))


if __name__ == '__main__':
    unittest.main()
