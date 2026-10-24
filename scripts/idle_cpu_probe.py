#!/usr/bin/env python3
"""Linux idle CPU/HTTP responsiveness regression probe (GH#354).

Attach to an isolated, already-running server. This process does not issue HTTP
requests during warmup or the idle window. Optional JSON workload cases run only
AFTER the idle window. Server configuration and lifecycle are never changed.
CPU percentages are interval deltas, with 100% meaning one logical CPU.
"""
from __future__ import annotations

import argparse
from dataclasses import dataclass
import json
import math
import os
from pathlib import Path
import sys
import time
from typing import Any
import urllib.error
import urllib.parse
import urllib.request
import uuid


@dataclass(frozen=True)
class TaskStat:
    name: str
    ticks: int
    started: int


def parse_stat(text: str) -> TaskStat:
    # comm can contain spaces and ')'; split AFTER the final closing parenthesis.
    left = text.index('(')
    right = text.rindex(')')
    fields = text[right + 1:].split()  # starts at stat field 3 (state)
    if fields[0] in ('Z', 'X', 'x'):
        raise ProcessLookupError('task has exited')
    return TaskStat(text[left + 1:right], int(fields[11]) + int(fields[12]),
                    int(fields[19]))


def snapshot(pid: int, proc: Path = Path('/proc')) -> dict[str, Any]:
    root = proc / str(pid)
    process = parse_stat((root / 'stat').read_text())
    threads = {}
    vanished = 0
    for directory in (root / 'task').iterdir():
        try:
            stat = parse_stat((directory / 'stat').read_text())
        except (FileNotFoundError, ProcessLookupError):
            vanished += 1
            continue
        try:
            wchan = (directory / 'wchan').read_text().strip()
        except OSError:
            wchan = None
        threads[(int(directory.name), stat.started)] = (stat, wchan)
    # Check identity again: PID reuse during enumeration must not become a pass.
    if parse_stat((root / 'stat').read_text()).started != process.started:
        raise RuntimeError('server PID changed while reading /proc')
    return {'at': time.monotonic(), 'process': process, 'threads': threads,
            'vanished_during_read': vanished}


def measure(pid: int, seconds: float, interval: float,
            worker_name: str = 'fsqlite-worker') -> dict[str, Any]:
    if not (math.isfinite(seconds) and math.isfinite(interval)
            and seconds > 0 and interval > 0):
        raise ValueError('duration and interval must be finite and positive')
    hz = os.sysconf('SC_CLK_TCK')
    first = previous = snapshot(pid)
    deadline = first['at'] + seconds
    totals: dict[tuple[int, int], dict[str, Any]] = {}
    samples = []
    vanished = first['vanished_during_read']
    # Retain the last observed tick count even if a task temporarily disappears
    # from an enumeration. Key by TID AND start time, never by thread name.
    last_ticks = {key: stat.ticks for key, (stat, _) in first['threads'].items()}
    last_seen = {key: first['at'] for key in first['threads']}
    while True:
        time.sleep(max(0.0, min(interval, deadline - time.monotonic())))
        current = snapshot(pid)
        if current['process'].started != first['process'].started:
            raise RuntimeError('server exited or PID was reused during measurement')
        elapsed = current['at'] - previous['at']
        process_delta = current['process'].ticks - previous['process'].ticks
        if elapsed <= 0 or process_delta < 0:
            raise RuntimeError('non-monotonic process CPU observation')
        hot = []
        for key, (stat, wchan) in current['threads'].items():
            # A newly discovered thread may predate the window; in that case
            # establish a baseline rather than count its lifetime CPU as idle.
            old = last_ticks.get(key)
            if old is None:
                # Conservatively do not assign unsampled lifetime CPU to this
                # window. Process CPU remains the authoritative complete total.
                old = stat.ticks
            delta = stat.ticks - old
            if delta < 0:
                raise RuntimeError('non-monotonic thread CPU observation')
            observed_for = current['at'] - last_seen.get(key, previous['at'])
            last_ticks[key] = stat.ticks
            last_seen[key] = current['at']
            total = totals.setdefault(key, {'tid': key[0], 'start_ticks': key[1],
                                           'name': stat.name, 'ticks': 0,
                                           'last_wchan': wchan})
            total['ticks'] += delta
            total['name'] = stat.name
            total['last_wchan'] = wchan
            hot.append({'tid': key[0], 'name': stat.name, 'cpu_percent_since_last_observation':
                        100.0 * delta / hz / observed_for, 'wchan': wchan})
        vanished += current['vanished_during_read']
        samples.append({'elapsed_seconds': current['at'] - first['at'],
                        'process_cpu_percent': 100.0 * process_delta / hz / elapsed,
                        'hottest_threads': sorted(hot, key=lambda t: t['cpu_percent_since_last_observation'],
                                                 reverse=True)[:8]})
        previous = current
        if current['at'] >= deadline:
            break
    elapsed = previous['at'] - first['at']
    ticks = previous['process'].ticks - first['process'].ticks
    threads = sorted(totals.values(), key=lambda t: t['ticks'], reverse=True)
    for thread in threads:
        thread['cpu_seconds_lower_bound'] = thread.pop('ticks') / hz
        thread['cpu_percent_lower_bound'] = (
            100.0 * thread['cpu_seconds_lower_bound'] / elapsed)
    workers = [t for t in threads if worker_name in t['name']]
    return {'pid': pid, 'process_start_ticks': first['process'].started,
            'clock_ticks_per_second': hz, 'elapsed_seconds': elapsed,
            'process_cpu_seconds': ticks / hz,
            'process_cpu_percent': 100.0 * ticks / hz / elapsed,
            'matched_worker_count': len(workers),
            'worker_cpu_percent_lower_bound': (
                sum(t['cpu_percent_lower_bound'] for t in workers) if workers else None),
            'thread_accounting': 'lower bound; process total includes unsampled/exited threads',
            'vanished_during_read': vanished, 'threads': threads, 'samples': samples,
            'probe_http_requests_during_idle': 0,
            'external_traffic_verified_absent': False}


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        raise urllib.error.HTTPError(req.full_url, code, 'redirect refused', headers, fp)


def run_workload(url: str, cases: list[dict[str, Any]], token: str,
                 timeout: float, max_latency_ms: float) -> list[dict[str, Any]]:
    parsed = urllib.parse.urlsplit(url)
    if (parsed.scheme != 'http' or parsed.hostname not in ('localhost', '127.0.0.1', '::1')
            or parsed.username or parsed.password or parsed.fragment):
        raise ValueError('workload URL must be a loopback HTTP URL without credentials')
    # Do not send a fixture bearer token to a proxy or redirect target.
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}), NoRedirect())
    results = []
    for index, case in enumerate(cases):
        name = str(case.get('name', f'case-{index + 1}'))
        headers = {'Accept': 'application/json', 'Content-Type': 'application/json'}
        if token:
            headers['Authorization'] = f'Bearer {token}'
        body = json.dumps(case['request']).encode()
        request = urllib.request.Request(url, data=body, headers=headers, method='POST')
        started = time.monotonic()
        try:
            with opener.open(request, timeout=timeout) as response:
                raw = response.read(4 * 1024 * 1024 + 1)
                if len(raw) > 4 * 1024 * 1024:
                    raise ValueError('response exceeded four MiB')
                payload = json.loads(raw)
                # A fast HTTP 200 carrying a JSON-RPC/tool error is a failure.
                if (not isinstance(payload, dict) or 'error' in payload
                        or 'result' not in payload
                        or payload.get('id') != case['request'].get('id')):
                    raise ValueError('missing result or JSON-RPC error')
                result = payload['result']
                if isinstance(result, dict) and result.get('isError'):
                    raise ValueError('tool returned isError')
                rendered = json.dumps(result, ensure_ascii=False)
                if not all(text in rendered for text in case['expect_contains']):
                    raise ValueError('required result content missing')
            elapsed_ms = (time.monotonic() - started) * 1000.0
            results.append({'name': name, 'latency_ms': elapsed_ms,
                            'passed': elapsed_ms <= max_latency_ms})
        except (OSError, ValueError) as error:
            # Avoid leaking request bodies, bearer tokens, or mailbox contents.
            results.append({'name': name, 'latency_ms':
                            (time.monotonic() - started) * 1000.0,
                            'passed': False, 'error_type': type(error).__name__})
            break  # Later cases may depend on this mutation having succeeded.
    return results


def expand_token(value: Any, token: str) -> Any:
    if isinstance(value, str):
        return value.replace('{{RUN_TOKEN}}', token)
    if isinstance(value, list):
        return [expand_token(item, token) for item in value]
    if isinstance(value, dict):
        return {key: expand_token(item, token) for key, item in value.items()}
    return value


def load_cases(path: Path, token: str = '') -> list[dict[str, Any]]:
    cases = json.loads(path.read_text())
    if token:
        cases = expand_token(cases, token)
    if not isinstance(cases, list) or not cases:
        raise ValueError('workload must be a nonempty JSON array')
    for case in cases:
        if (not isinstance(case, dict) or not isinstance(case.get('request'), dict)
                or case['request'].get('jsonrpc') != '2.0'
                or type(case['request'].get('id')) not in (int, str)
                or not isinstance(case['request'].get('method'), str)
                or not case['request']['method']
                or not isinstance(case.get('expect_contains'), list)
                or not case['expect_contains']
                or not all(isinstance(t, str) and t for t in case['expect_contains'])):
            raise ValueError('each case needs a JSON-RPC request with id/method and nonempty expect_contains')
    return cases


def finite_positive(value: str) -> float:
    number = float(value)
    if not math.isfinite(number) or number <= 0:
        raise argparse.ArgumentTypeError('must be finite and positive')
    return number


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--pid', type=int, required=True)
    parser.add_argument('--seconds', type=finite_positive, default=60.0)
    parser.add_argument('--interval', type=finite_positive, default=1.0)
    parser.add_argument('--warmup', type=finite_positive, default=30.0)
    parser.add_argument('--max-idle-cpu-percent', type=finite_positive, default=10.0)
    parser.add_argument('--worker-name', default='fsqlite-worker')
    parser.add_argument('--url', help='loopback JSON-RPC endpoint for post-idle workload')
    parser.add_argument('--workload', type=Path, help='JSON cases with request/expect_contains')
    parser.add_argument('--token-env', default='HTTP_BEARER_TOKEN')
    parser.add_argument('--run-token', default='idlecpu' + uuid.uuid4().hex,
                        help='unique message/search token; reuse only for restart read checks')
    parser.add_argument('--request-timeout', type=finite_positive, default=10.0)
    parser.add_argument('--max-latency-ms', type=finite_positive, default=5000.0)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    if args.pid <= 0 or bool(args.url) != bool(args.workload):
        parser.error('positive PID required; --url and --workload must be supplied together')
    if not (args.run_token.isascii() and args.run_token.isalnum()
            and len(args.run_token) <= 128):
        parser.error('--run-token must contain 1..128 ASCII letters/digits')
    report: dict[str, Any] = {'schema_version': 1, 'passed': False,
                              'run_token': args.run_token,
                              'workload_status': 'not_requested',
                              'warmup_seconds': args.warmup,
                              'request_timeout_seconds': args.request_timeout,
                              'max_latency_ms': args.max_latency_ms}
    try:
        cases = load_cases(args.workload, args.run_token) if args.workload else []
        identity = snapshot(args.pid)['process'].started
        time.sleep(args.warmup)
        if snapshot(args.pid)['process'].started != identity:
            raise RuntimeError('server PID changed during warmup')
        idle = measure(args.pid, args.seconds, args.interval, args.worker_name)
        if idle['process_start_ticks'] != identity:
            raise RuntimeError('server PID changed before measurement')
        report['idle'] = idle
        report['idle_threshold_percent'] = args.max_idle_cpu_percent
        report['idle_passed'] = idle['process_cpu_percent'] <= args.max_idle_cpu_percent
        if cases:
            report['workload'] = run_workload(args.url, cases,
                os.environ.get(args.token_env, ''), args.request_timeout, args.max_latency_ms)
            report['workload_status'] = 'passed' if (
                len(report['workload']) == len(cases)
                and all(case['passed'] for case in report['workload'])) else 'failed'
        if snapshot(args.pid)['process'].started != identity:
            raise RuntimeError('server PID changed during workload')
        report['passed'] = report['idle_passed'] and report['workload_status'] != 'failed'
        exit_code = 0 if report['passed'] else 1
    except (OSError, ValueError, RuntimeError) as error:
        report['inconclusive'] = True
        report['error_type'] = type(error).__name__
        exit_code = 2
    args.output.write_text(json.dumps(report, indent=2, allow_nan=False) + '\n')
    print(json.dumps({'passed': report['passed'], 'report': str(args.output),
                      'exit_code': exit_code}))
    return exit_code


if __name__ == '__main__':
    sys.exit(main())
