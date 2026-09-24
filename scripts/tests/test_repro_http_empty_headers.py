"""Tests of the probe itself, NOT execution of Agent Mail/FastMCP."""
import contextlib
import importlib.util
import io
import json
from pathlib import Path
import socket
import threading
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location(
    'probe', Path(__file__).resolve().parents[1] / 'repro_http_empty_headers.py',
)
assert spec is not None and spec.loader is not None
probe = importlib.util.module_from_spec(spec)
spec.loader.exec_module(probe)


class ProbeHarnessTests(unittest.TestCase):
    def test_wire_preserves_empty_and_ows_values(self):
        for value in (b'', b' \t ', b'\t'):
            wire = probe.request_bytes(b'127.0.0.1:8765', b'/mcp/', [(b'Tailscale-User-Profile-Pic', value)])
            self.assertIn(b'\r\nTailscale-User-Profile-Pic:' + value + b'\r\n', wire)
            self.assertTrue(wire.endswith(b'\r\n\r\n' + probe.BODY))

    def test_negative_fields_are_not_filtered_by_client(self):
        wire = probe.request_bytes(b'localhost', b'/mcp/', [(b'Bad Header', b'a\x00b\x7fc\rd')])
        self.assertIn(b'Bad Header:a\x00b\x7fc\rd\r\n', wire)

    def test_blank_required_field_replaces_default(self):
        wire = probe.request_bytes(b'localhost', b'/mcp/', [(b'cOnTeNt-TyPe', b'')])
        self.assertEqual(wire.lower().count(b'content-type:'), 1)
        self.assertIn(b'cOnTeNt-TyPe:\r\n', wire)

    def test_empty_transfer_encoding_does_not_test_cl_te_conflict(self):
        wire = probe.request_bytes(b'localhost', b'/mcp/', [(b'Transfer-Encoding', b'')])
        self.assertNotIn(b'Content-Length:', wire)
        self.assertIn(b'Transfer-Encoding:\r\n', wire)

    def fixture_run(self, bug, expect_bug=False, permissive_required=False):
        listener = socket.socket()
        listener.bind(('127.0.0.1', 0))
        listener.listen()
        listener.settimeout(5)
        host, port = listener.getsockname()
        captured = []
        errors = []
        required = {b'authorization', b'content-type', b'content-length', b'transfer-encoding', b'host', b'origin'}
        def serve():
            try:
                for _ in range(23):
                    with listener.accept()[0] as connection:
                        connection.settimeout(3)
                        data = b''
                        while b'\r\n\r\n' not in data:
                            chunk = connection.recv(4096)
                            if not chunk:
                                raise RuntimeError('unexpected EOF')
                            data += chunk
                        head, body = data.split(b'\r\n\r\n', 1)
                        while len(body) < len(probe.BODY):
                            chunk = connection.recv(4096)
                            if not chunk:
                                raise RuntimeError('unexpected EOF in request body')
                            body += chunk
                        captured.append(head)
                        invalid = False
                        for line in head.split(b'\r\n')[1:]:
                            name, value = line.split(b':', 1)
                            value = value.strip(b' \t')
                            invalid |= b' ' in name or any((b < 32 and b != 9) or b == 127 for b in value)
                            if not value:
                                invalid |= bug or (not permissive_required and name.lower() in required)
                        status = 400 if invalid else 200
                        message = ({'error': {'code': -32600, 'message': 'invalid HTTP header'}} if invalid else {
                            'jsonrpc': '2.0', 'id': 1, 'result': {
                                'serverInfo': {'name': 'SYNTHETIC HARNESS FIXTURE', 'version': 'fixture-only'},
                                'protocolVersion': '2024-11-05', 'capabilities': {},
                            },
                        })
                        payload = json.dumps(message).encode()
                        connection.sendall(f'HTTP/1.1 {status} Fixture\r\nContent-Type: application/json\r\nContent-Length: {len(payload)}\r\nConnection: close\r\n\r\n'.encode() + payload)
            except BaseException as error:
                errors.append(error)
            finally:
                listener.close()
        thread = threading.Thread(target=serve, daemon=True)
        thread.start()
        args = ['probe', '--url', f'http://{host}:{port}/mcp/']
        if expect_bug:
            args.append('--expect-bug')
        output = io.StringIO()
        with patch('sys.argv', args), contextlib.redirect_stdout(output):
            code = probe.main()
        thread.join(timeout=8)
        self.assertFalse(thread.is_alive())
        self.assertEqual(errors, [])
        self.assertEqual(len(captured), 23)
        return code, output.getvalue()

    def test_reported_mode_detects_bug_fixture(self):
        code, output = self.fixture_run(bug=True, expect_bug=True)
        self.assertEqual(code, 0, output)
        self.assertIn('22/22 cases passed', output)

    def test_regression_mode_rejects_bug_fixture(self):
        code, output = self.fixture_run(bug=True)
        self.assertEqual(code, 1, output)
        self.assertIn('17/22 cases passed', output)

    def test_regression_mode_accepts_fixed_fixture(self):
        code, output = self.fixture_run(bug=False)
        self.assertEqual(code, 0, output)
        self.assertIn('22/22 cases passed', output)

    def test_reported_mode_rejects_fixed_fixture(self):
        code, output = self.fixture_run(bug=False, expect_bug=True)
        self.assertEqual(code, 1, output)
        self.assertIn('17/22 cases passed', output)

    def test_regression_mode_rejects_blank_required_fields(self):
        code, output = self.fixture_run(bug=False, permissive_required=True)
        self.assertEqual(code, 1, output)
        self.assertIn('10/22 cases passed', output)

    def test_http_200_is_not_enough_without_initialize_result(self):
        for body in (b'{}', b'{"jsonrpc":"2.0","id":1,"result":{}}', b'not JSON'):
            self.assertIsNone(probe.initialized(200, body))

    def test_transport_exception_is_not_counted_as_rejection(self):
        baseline = json.dumps({'jsonrpc': '2.0', 'id': 1, 'result': {'serverInfo': {}, 'protocolVersion': 'test'}}).encode()
        output = io.StringIO()
        with patch('sys.argv', ['probe']), patch.object(probe, 'exchange', side_effect=[(200, baseline)] + [TimeoutError()] * 22), contextlib.redirect_stdout(output):
            self.assertEqual(probe.main(), 1)
        self.assertIn('0/22 cases passed', output.getvalue())


if __name__ == '__main__':
    unittest.main(verbosity=2)
