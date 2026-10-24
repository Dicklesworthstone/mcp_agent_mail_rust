"""Tests of the release smoke's swarm-phase judgement logic (br-kp1in.34).

These exercise the pure helpers only, NOT Agent Mail; the phase itself runs
against real binaries through tests/e2e/test_release_smoke.sh.
"""
import importlib.util
from pathlib import Path
import sys
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
