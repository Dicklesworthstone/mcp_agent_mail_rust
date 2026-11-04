#!/usr/bin/env python3
"""Run paired GH372 probes on fresh, exclusively owned synthetic copies."""
import argparse
import hashlib
import json
from pathlib import Path
import platform
import re
import shutil
import sqlite3
import subprocess
import sys
import time


def file_sha256(path):
    with path.open('rb') as source:
        return hashlib.file_digest(source, 'sha256').hexdigest()


def cgroup_snapshot():
    """Cgroups are contextual diagnostics, never a required memory oracle."""
    values = {}
    for name in ['memory.events', 'memory.current', 'memory.peak', 'memory.max']:
        try:
            values[name] = (Path('/sys/fs/cgroup') / name).read_text()
        except OSError as error:
            values[name] = {'unavailable': str(error)}
    return values


def parse_specs(specifications, kind):
    parsed = []
    names = set()
    for specification in specifications:
        name, value = specification.split('=', 1)
        assert re.fullmatch(r'[A-Za-z0-9_-]+', name), f'invalid {kind} label: {name}'
        assert name not in names, f'duplicate {kind} label: {name}'
        names.add(name)
        path = Path(value).resolve(strict=True)
        assert path.is_file(), f'missing {kind}: {path}'
        if kind == 'seed':
            assert path.parent.name.startswith('am-gh372-memory-')
        parsed.append((name, path))
    return parsed


def image_witness(connection, expected_update=False):
    """Hash every ordered table row, accounting only for the requested UPDATE."""
    schema = connection.execute(
        'SELECT type,name,tbl_name,sql FROM sqlite_master ORDER BY type,name'
    ).fetchall()
    tables = {}
    for (name,) in connection.execute(
        "SELECT name FROM sqlite_master WHERE type='table' ORDER BY name"
    ).fetchall():
        identifier = '"' + name.replace('"', '""') + '"'
        cursor = connection.execute(f'SELECT * FROM {identifier} ORDER BY rowid')
        columns = [column[0] for column in cursor.description]
        digest = hashlib.sha256()
        count = 0
        for row in cursor:
            row = list(row)
            if expected_update and name == 'messages' and row[columns.index('id')] == 1:
                row[columns.index('subject')] = 'post-upgrade write'
            encoded = [
                {'sqlite_blob_hex': value.hex()} if isinstance(value, bytes) else value
                for value in row
            ]
            digest.update(json.dumps(encoded, ensure_ascii=False, separators=(',', ':')).encode())
            digest.update(b'\n')
            count += 1
        tables[name] = {'rows': count, 'sha256': digest.hexdigest()}
    return {'schema': schema, 'tables': tables}


def run_logged(command, path, timeout):
    started = time.monotonic()
    try:
        run = subprocess.run(command, capture_output=True, timeout=timeout)
    except subprocess.TimeoutExpired as error:
        path.write_bytes((error.stdout or b'') + (error.stderr or b'')
                         + f'\nPROBE_TIMEOUT seconds={timeout}\n'.encode())
        raise
    elapsed = time.monotonic() - started
    log = run.stdout + run.stderr
    path.write_bytes(log)
    assert run.returncode == 0, f'probe failed: {path.name}: {run.returncode}'
    return log, elapsed


def verify_copy(path, expected):
    # The native process explicitly joins its workers before this independent
    # stock-SQLite read-only connection opens the exclusively owned copy.
    verify = sqlite3.connect(path.as_uri() + '?mode=ro', uri=True)
    try:
        assert verify.execute('PRAGMA integrity_check').fetchall() == [('ok',)]
        assert verify.execute('PRAGMA foreign_key_check').fetchall() == []
        assert verify.execute('SELECT subject,body_md FROM messages WHERE id=1').fetchone() == (
            'post-upgrade write', 'retained body ' * 320
        )
        totals = verify.execute('SELECT COUNT(*),SUM(length(body_md)) FROM messages').fetchone()
        full_witness = image_witness(verify)
        assert full_witness == expected, f'contents changed unexpectedly: {path}'
        return totals, full_witness
    finally:
        verify.close()


def main():
    if not __debug__:
        raise RuntimeError('run this verifier with python3 -I so assertions remain enabled')
    parser = argparse.ArgumentParser()
    parser.add_argument('--binary', action='append', required=True, help='label=/absolute/probe')
    parser.add_argument('--seed', action='append', required=True, help='label=/absolute/seed.sqlite3')
    parser.add_argument('--rounds', type=int, default=1)
    parser.add_argument('--output', required=True)
    parser.add_argument('--fixtures-dir', help='separate new directory for bulky disposable copies')
    parser.add_argument('--primed', action='store_true')
    parser.add_argument('--backend', action='append', choices=['adapter', 'async', 'raw'],
                        help='repeat for controls; defaults to the application adapter')
    parser.add_argument('--timeout', type=int, default=300)
    args = parser.parse_args()
    assert args.rounds > 0 and args.timeout > 0
    all_binaries = parse_specs(args.binary, 'binary')
    seeds = parse_specs(args.seed, 'seed')
    backends = args.backend or ['adapter']
    assert len(backends) == len(set(backends))
    output = Path(args.output).resolve()
    output.mkdir(mode=0o700, parents=True, exist_ok=False)
    fixture_root = output
    if args.fixtures_dir:
        fixture_root = Path(args.fixtures_dir).resolve()
        fixture_root.mkdir(mode=0o700, parents=True, exist_ok=False)
    results = []
    expected = {}
    seed_hashes = {}
    binary_hashes = {label: file_sha256(path) for label, path in all_binaries}
    for seed_name, seed in seeds:
        seed_hashes[seed_name] = file_sha256(seed)
        source = sqlite3.connect(seed.as_uri() + '?mode=ro', uri=True)
        try:
            assert source.execute('PRAGMA integrity_check').fetchall() == [('ok',)]
            assert source.execute('PRAGMA foreign_key_check').fetchall() == []
            expected[seed_name] = image_witness(source, expected_update=True)
        finally:
            source.close()
        assert file_sha256(seed) == seed_hashes[seed_name]
    (output / 'provenance.json').write_text(json.dumps({
        'command': sys.argv,
        'platform': platform.platform(),
        'python_version': sys.version,
        'sqlite_version': sqlite3.sqlite_version,
        'binaries': {label: {'path': str(path), 'sha256': binary_hashes[label]}
                     for label, path in all_binaries},
        'seeds': {label: {'path': str(path), 'sha256': seed_hashes[label],
                          'bytes': path.stat().st_size} for label, path in seeds},
        'memory_metric': 'max(0, subsequent VmHWM - baseline VmRSS), in KiB',
        'budget_kib': 32 * 1024,
        'primed': args.primed,
    }, indent=2) + '\n')
    for repetition in range(args.rounds):
        # Alternate paired order to avoid assigning only the coldest invocation
        # to the same binary. Each invocation still uses its own pristine file.
        binaries = all_binaries if repetition % 2 == 0 else list(reversed(all_binaries))
        for seed_name, seed in seeds:
            for count in [1, 5]:
                for backend in backends:
                    for label, binary in binaries:
                        name = f'{label}-{seed_name}-{backend}-{count}-{repetition}'
                        directory = fixture_root / f'am-gh372-memory-{name}'
                        directory.mkdir(mode=0o700)
                        path = directory / 'storage.sqlite3'
                        shutil.copy2(seed, path)
                        assert file_sha256(path) == seed_hashes[seed_name]
                        command = [str(binary), str(path), str(count), backend, 'mailbox', 'detailed']
                        if args.primed:
                            run_logged([str(binary), str(path), '1', backend, 'open'],
                                       output / f'{name}-prime.log', args.timeout)
                        before = cgroup_snapshot()
                        log_path = output / f'{name}.log'
                        log, elapsed = run_logged(command, log_path, args.timeout)
                        after = cgroup_snapshot()
                        matches = re.findall(rb'^RESULT (.*)$', log, flags=re.M)
                        assert len(matches) == 1, f'missing or duplicate native result: {name}'
                        measured = {key.decode(): int(value)
                                    for key, value in re.findall(rb'(\w+)=(\d+)', matches[0])}
                        assert set(measured) == {'baseline_kib', 'initial_peak_kib', 'final_rss_kib',
                                                 'peak_kib', 'delta_kib', 'live_delta_kib'}
                        assert measured['delta_kib'] == max(0, measured['peak_kib'] - measured['baseline_kib'])
                        witness, full_witness = verify_copy(path, expected[seed_name])
                        record = {'label': label, 'seed': seed_name, 'backend': backend,
                                  'connections': count, 'round': repetition,
                                  'command': command, 'binary_sha256': binary_hashes[label],
                                  'elapsed_seconds': elapsed, 'seed_file_bytes': seed.stat().st_size,
                                  'seed_sha256': seed_hashes[seed_name],
                                  'measured': measured,
                                  'within_32_mib_budget': measured['delta_kib'] <= 32 * 1024,
                                  'canonical_count_body_bytes': witness,
                                  'canonical_full_content_witness': full_witness,
                                  'cgroup_before': before, 'cgroup_after': after,
                                  'log': str(log_path)}
                        results.append(record)
                        (output / 'results.json').write_text(json.dumps(results, indent=2) + '\n')
                        print(json.dumps({key: record[key] for key in
                                          ['label', 'seed', 'backend', 'connections', 'round',
                                           'measured', 'within_32_mib_budget']}), flush=True)
    for seed_name, seed in seeds:
        assert file_sha256(seed) == seed_hashes[seed_name], f'pristine seed changed: {seed_name}'
    for label, binary in all_binaries:
        assert file_sha256(binary) == binary_hashes[label], f'binary changed during matrix: {label}'


if __name__ == '__main__':
    main()
