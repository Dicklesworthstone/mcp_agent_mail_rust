"""Probe self-tests; these are not Agent Mail performance acceptance results."""
import importlib.util
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import threading
import time
import unittest
from unittest.mock import patch

SCRIPT = Path(__file__).resolve().parents[1] / 'scripts' / 'idle_cpu_probe.py'
spec = importlib.util.spec_from_file_location('idle_cpu_probe', SCRIPT)
probe = importlib.util.module_from_spec(spec)
sys.modules[spec.name] = probe
spec.loader.exec_module(probe)


def observation(at, process_ticks, threads, started=1):
    return {'at': at, 'process': probe.TaskStat('am', process_ticks, started),
            'threads': threads, 'vanished_during_read': 0}


class AccountingTests(unittest.TestCase):
    def test_stat_uses_own_cpu_not_children_and_accepts_parentheses(self):
        # Fields 14/15 are user/system; 16/17 are deliberately huge child times.
        fields = ['S'] + ['0'] * 30
        fields[11:15] = ['13', '17', '100000', '200000']
        fields[19] = '1234'
        got = probe.parse_stat('42 (fsqlite ) worker) ' + ' '.join(fields))
        self.assertEqual(got, probe.TaskStat('fsqlite ) worker', 30, 1234))

    def test_zombie_is_not_a_quiet_live_server(self):
        with self.assertRaises(ProcessLookupError):
            probe.parse_stat('42 (am) Z ' + '0 ' * 30)

    def test_unique_run_token_replaces_both_request_and_assertion(self):
        value = {'request': {'id': 1, 'query': '{{RUN_TOKEN}}'},
                 'expect_contains': ['{{RUN_TOKEN}}']}
        first = probe.expand_token(value, 'idlecpu1')
        second = probe.expand_token(value, 'idlecpu2')
        self.assertEqual(first['request']['query'], 'idlecpu1')
        self.assertEqual(first['expect_contains'], ['idlecpu1'])
        self.assertNotEqual(first, second)
        self.assertEqual(value['request']['query'], '{{RUN_TOKEN}}')

    def measured(self, snapshots):
        with patch.object(probe, 'snapshot', side_effect=snapshots), \
                patch.object(probe.time, 'sleep'), \
                patch.object(probe.time, 'monotonic', return_value=10), \
                patch.object(probe.os, 'sysconf', return_value=100):
            return probe.measure(42, 2, 1)

    def test_cpu_is_window_delta_not_lifetime_average(self):
        key = (43, 2)
        result = self.measured([
            observation(10, 50000, {key: (probe.TaskStat('fsqlite-worker', 9000, 2), '0')}),
            observation(11, 50025, {key: (probe.TaskStat('fsqlite-worker', 9025, 2), '0')}),
            observation(12, 50050, {key: (probe.TaskStat('fsqlite-worker', 9050, 2), '0')}),
        ])
        self.assertEqual(result['process_cpu_percent'], 25)
        self.assertEqual(result['worker_cpu_percent_lower_bound'], 25)
        self.assertEqual(result['probe_http_requests_during_idle'], 0)

    def test_tid_reuse_and_new_thread_lifetime_do_not_inflate_attribution(self):
        old, new = (43, 2), (43, 3)
        result = self.measured([
            observation(10, 1000, {old: (probe.TaskStat('fsqlite-worker', 500, 2), '0')}),
            observation(11, 1300, {new: (probe.TaskStat('fsqlite-worker', 250, 3), '0')}),
            observation(12, 1320, {new: (probe.TaskStat('fsqlite-worker', 270, 3), '0')}),
        ])
        self.assertEqual(result['process_cpu_percent'], 160)
        self.assertEqual(result['worker_cpu_percent_lower_bound'], 10)

    def test_exited_thread_cpu_remains_in_process_total(self):
        result = self.measured([
            observation(10, 100, {}), observation(11, 150, {}),
            observation(12, 200, {})])
        self.assertEqual(result['process_cpu_percent'], 50)
        self.assertIsNone(result['worker_cpu_percent_lower_bound'])
        self.assertEqual(result['matched_worker_count'], 0)

    def test_missing_observation_does_not_double_count_task(self):
        key = (43, 2)
        result = self.measured([
            observation(10, 100, {key: (probe.TaskStat('fsqlite-worker', 50, 2), '0')}),
            observation(11, 150, {}),
            observation(12, 200, {key: (probe.TaskStat('fsqlite-worker', 150, 2), '0')}),
        ])
        self.assertEqual(result['worker_cpu_percent_lower_bound'], 50)
        hot = result['samples'][-1]['hottest_threads'][0]
        self.assertEqual(hot['cpu_percent_since_last_observation'], 50)

    def test_pid_reuse_and_backwards_cpu_fail_closed(self):
        for changed in (observation(11, 100, {}, started=2), observation(11, 99, {})):
            with self.assertRaises(RuntimeError):
                self.measured([observation(10, 100, {}), changed])

    def test_invalid_intervals_are_rejected(self):
        for duration, interval in ((0, 1), (1, 0), (float('nan'), 1), (1, float('inf'))):
            with self.assertRaises(ValueError):
                probe.measure(42, duration, interval)

    def test_workload_requires_nonempty_correctness_assertions(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / 'workload.json'
            for content in ([], [{'request': {}}], [{'request': {}, 'expect_contains': ['']} ]):
                path.write_text(json.dumps(content))
                with self.assertRaises(ValueError):
                    probe.load_cases(path)


class WorkloadTests(unittest.TestCase):
    def setUp(self):
        self.requests = []
        self.request_times = []
        self.reply = {'jsonrpc': '2.0', 'id': 1, 'result': {'content': 'probe-token'}}
        self.redirect = False
        owner = self

        class Handler(BaseHTTPRequestHandler):
            def do_POST(self):
                owner.request_times.append(time.monotonic())
                owner.requests.append(json.loads(self.rfile.read(int(self.headers['Content-Length']))))
                if owner.redirect:
                    self.send_response(302)
                    self.send_header('Location', 'http://127.0.0.1:1/never')
                    self.end_headers()
                else:
                    body = json.dumps(owner.reply).encode()
                    self.send_response(200)
                    self.send_header('Content-Length', str(len(body)))
                    self.end_headers()
                    self.wfile.write(body)

            def log_message(self, *args):
                pass

        self.server = ThreadingHTTPServer(('127.0.0.1', 0), Handler)
        self.thread = threading.Thread(target=self.server.serve_forever)
        self.thread.start()
        self.url = f'http://127.0.0.1:{self.server.server_port}/api/'
        self.cases = [{'name': 'read', 'request': {'jsonrpc': '2.0', 'id': 1, 'method': 'tools/call'},
                       'expect_contains': ['probe-token']}]

    def tearDown(self):
        self.server.shutdown()
        self.thread.join()
        self.server.server_close()

    def test_fast_correct_response_passes(self):
        result = probe.run_workload(self.url, self.cases, 'secret', 2, 2000)
        self.assertTrue(result[0]['passed'])
        self.assertEqual(len(self.requests), 1)
        self.assertNotIn('secret', json.dumps(result))

    def test_http_200_rpc_tool_and_content_errors_fail(self):
        for reply in ({'error': {'code': -1}}, {'result': {'isError': True}},
                      {'result': 'wrong-token'}, {'unrelated': 'probe-token'},
                      {'id': 2, 'result': 'probe-token'}):
            self.reply = reply
            result = probe.run_workload(self.url, self.cases * 2, '', 2, 2000)
            self.assertFalse(result[0]['passed'])
            self.assertEqual(len(result), 1)

    def test_slow_correct_response_fails_latency_gate(self):
        result = probe.run_workload(self.url, self.cases, '', 2, 0.000001)
        self.assertFalse(result[0]['passed'])

    def test_redirects_and_nonlocal_destinations_are_refused(self):
        self.redirect = True
        self.assertFalse(probe.run_workload(self.url, self.cases, 'secret', 2, 2000)[0]['passed'])
        for url in ('http://example.com/', 'http://user:pass@localhost/', 'https://localhost/'):
            with self.assertRaises(ValueError):
                probe.run_workload(url, self.cases, 'secret', 2, 2000)

    @unittest.skipUnless(sys.platform.startswith('linux'), 'requires Linux /proc')
    def test_real_idle_then_workload_phase_order_and_report(self):
        with subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(60)']) as child, \
                tempfile.TemporaryDirectory() as directory:
            try:
                root = Path(directory)
                workload, report = root / 'workload.json', root / 'report.json'
                workload.write_text(json.dumps(self.cases))
                command = [sys.executable, str(SCRIPT), '--pid', str(child.pid),
                           '--warmup', '0.1', '--seconds', '0.2', '--interval', '0.1',
                           '--max-idle-cpu-percent', '20', '--url', self.url,
                           '--workload', str(workload), '--output', str(report)]
                began = time.monotonic()
                completed = subprocess.run(command, capture_output=True, timeout=10)
                self.assertEqual(completed.returncode, 0, completed.stderr)
                payload = json.loads(report.read_text())
                self.assertTrue(payload['passed'])
                self.assertEqual(payload['workload_status'], 'passed')
                self.assertEqual(payload['idle']['probe_http_requests_during_idle'], 0)
                self.assertEqual(len(self.requests), 1)
                self.assertGreaterEqual(self.request_times[0] - began, 0.3)
            finally:
                child.terminate()
                child.wait(timeout=5)


@unittest.skipUnless(sys.platform.startswith('linux'), 'requires Linux /proc')
class LinuxCalibrationTests(unittest.TestCase):
    def test_sleeping_and_spinning_processes_are_distinguished(self):
        measured = {}
        for label, work in [('sleep', 'time.sleep(60)'), ('spin', 'exec("while True: pass")')]:
            code = f'import time; print("ready", flush=True); {work}'
            child = subprocess.Popen([sys.executable, '-c', code], stdout=subprocess.PIPE)
            try:
                self.assertEqual(child.stdout.readline(), b'ready\n')
                measured[label] = probe.measure(child.pid, 0.6, 0.2)['process_cpu_percent']
            finally:
                child.terminate()
                child.wait(timeout=5)
                child.stdout.close()
        self.assertLess(measured['sleep'], 20, measured)
        self.assertGreater(measured['spin'], measured['sleep'] + 10, measured)

    def test_busy_process_fails_the_cli_cpu_gate(self):
        child = subprocess.Popen([sys.executable, '-c',
                                  'print("ready", flush=True); exec("while True: pass")'],
                                 stdout=subprocess.PIPE)
        try:
            self.assertEqual(child.stdout.readline(), b'ready\n')
            with tempfile.TemporaryDirectory() as directory:
                output = Path(directory) / 'report.json'
                completed = subprocess.run([sys.executable, str(SCRIPT), '--pid', str(child.pid),
                    '--warmup', '0.01', '--seconds', '0.6', '--interval', '0.2',
                    '--max-idle-cpu-percent', '1', '--output', str(output)],
                    capture_output=True, timeout=5)
                self.assertEqual(completed.returncode, 1, completed.stderr)
                self.assertFalse(json.loads(output.read_text())['passed'])
        finally:
            child.terminate()
            child.wait(timeout=5)
            child.stdout.close()

    def test_missing_process_is_inconclusive_not_a_cpu_pass(self):
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory) / 'report.json'
            # Linux pid_max is below this signed-32-bit value.
            completed = subprocess.run([sys.executable, str(SCRIPT), '--pid', '2147483647',
                                        '--warmup', '0.01', '--output', str(output)],
                                       capture_output=True, timeout=5)
            self.assertEqual(completed.returncode, 2)
            report = json.loads(output.read_text())
            self.assertFalse(report['passed'])
            self.assertTrue(report['inconclusive'])


if __name__ == '__main__':
    unittest.main()
