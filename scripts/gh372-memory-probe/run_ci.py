#!/usr/bin/env python3
"""Native GH#372 diagnostic qualification on an exclusively owned CI workdir.

The 32 MiB diagnostic gate preserves VmHWM minus initial VmRSS. This standalone
SQLModel workload is evidence for the application regression, not execution of
the repository's integration test: that and wider release gates remain pending.
No operational mailbox, main-branch commit, or production deployment is used.
"""

import argparse
import collections
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import platform
import re
import shutil
import signal
import subprocess
import sys
import tarfile
import time
import tomllib
import urllib.request


HERE = Path(__file__).resolve().parent
UA = 'mcp_agent_mail_rust-agent (+https://github.com/Dicklesworthstone/mcp_agent_mail_rust)'
PACKAGE = ('fsqlite', '0.4.13')
BUDGET_KIB = 32 * 1024
SOURCE_FILES = ('Cargo.toml', 'Cargo.lock', 'src/main.rs',
                'src/bin/future_sizes.rs', 'tests/preservation.rs')


def sha256(path):
    with path.open('rb') as source:
        return hashlib.file_digest(source, 'sha256').hexdigest()


def write_json(path, value):
    path.write_text(json.dumps(value, indent=2, sort_keys=True) + '\n')


def owned_directory(path):
    """Refuse every pre-existing output; never clean or reuse someone else's work."""
    path = path.absolute()
    assert path.parent.resolve(strict=True) == path.parent, 'workdir parent must be canonical'
    path.mkdir(mode=0o700, exist_ok=False)
    return path


def stop_owned_group(process):
    # Only a child created here with start_new_session=True is accepted.
    # Its session/process-group id is its pid, including Rust's crash-test child.
    for sig, grace in ((signal.SIGTERM, 5), (signal.SIGKILL, 5)):
        try:
            os.killpg(process.pid, sig)
        except ProcessLookupError:
            break
        try:
            process.wait(timeout=grace)
        except subprocess.TimeoutExpired:
            continue
        # A dead group leader can leave descendants; still send the final kill
        # to this owned group rather than treating leader exit as group cleanup.
        if sig == signal.SIGKILL:
            break
    process.wait(timeout=10)


class Commands:
    def __init__(self, evidence):
        self.evidence = evidence
        self.logs = evidence / 'logs'
        self.logs.mkdir()
        self.records = []

    def run(self, name, command, *, env, cwd=None, timeout=600, required=True):
        stdout = self.logs / f'{name}.stdout.log'
        stderr = self.logs / f'{name}.stderr.log'
        command = [str(value) for value in command]
        record = {'name': name, 'command': command, 'cwd': str(cwd) if cwd else None,
                  'timeout_seconds': timeout, 'stdout': str(stdout), 'stderr': str(stderr)}
        self.records.append(record)
        write_json(self.evidence / 'commands.json', self.records)
        print(f'RUN {name}', flush=True)
        started = time.monotonic()
        process = None
        try:
            with stdout.open('xb') as out, stderr.open('xb') as err:
                process = subprocess.Popen(command, cwd=cwd, env=env, stdout=out, stderr=err,
                                           start_new_session=True)
                record['pid'] = process.pid
                try:
                    record['returncode'] = process.wait(timeout=timeout)
                except subprocess.TimeoutExpired:
                    record['timed_out'] = True
                    stop_owned_group(process)
                    record['returncode'] = process.returncode
        except BaseException:
            if process is not None and process.poll() is None:
                stop_owned_group(process)
            raise
        finally:
            record['elapsed_seconds'] = time.monotonic() - started
            write_json(self.evidence / 'commands.json', self.records)
        record['success'] = record.get('returncode') == 0 and not record.get('timed_out', False)
        write_json(self.evidence / 'commands.json', self.records)
        print(f"DONE {name} exit={record.get('returncode')} seconds={record['elapsed_seconds']:.2f}",
              flush=True)
        if required and not record['success']:
            raise RuntimeError(f'{name} failed; inspect {stdout} and {stderr}')
        return record


def download(url, expected_sha, path):
    assert url.startswith('https://') and re.fullmatch('[0-9a-f]{64}', expected_sha)
    request = urllib.request.Request(url, headers={'User-Agent': UA})
    with urllib.request.urlopen(request, timeout=120) as response, path.open('xb') as output:
        shutil.copyfileobj(response, output)
    assert sha256(path) == expected_sha, f'archive digest mismatch: {url}'
    return path


def extract_archive(archive, destination, *, prefix=None, only_member=None):
    destination.mkdir(mode=0o700)
    seen = set()
    regular = []
    with tarfile.open(archive, 'r:*') as source:
        for member in source.getmembers():
            parts = PurePosixPath(member.name).parts
            assert parts and not member.name.startswith('/') and '\\' not in member.name
            assert '..' not in parts and member.name not in seen
            assert member.isdir() or member.isfile(), f'unsupported archive entry: {member.name}'
            if prefix:
                assert parts[0] == prefix and (len(parts) > 1 or member.isdir())
            if only_member:
                assert member.name == only_member and member.isfile()
            seen.add(member.name)
            target = destination.joinpath(*parts)
            if member.isdir():
                target.mkdir(mode=0o700, parents=True, exist_ok=True)
                continue
            target.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
            contents = source.extractfile(member)
            assert contents is not None
            with contents, target.open('xb') as output:
                shutil.copyfileobj(contents, output)
            assert target.stat().st_size == member.size
            regular.append(target)
    assert regular and (only_member is None or len(regular) == 1)
    return destination / prefix if prefix else regular[0]


def tree_hashes(root):
    return {str(path.relative_to(root)): sha256(path)
            for path in sorted(root.rglob('*')) if path.is_file()}


def assert_lock_delta(original, candidate, evidence_path):
    before = tomllib.loads(original.read_text())
    after = tomllib.loads(candidate.read_text())
    before_packages = {(p['name'], p['version']): p for p in before['package']}
    after_packages = {(p['name'], p['version']): p for p in after['package']}
    assert len(before_packages) == len(before['package'])
    assert len(after_packages) == len(after['package'])
    assert before_packages.keys() == after_packages.keys(), 'dependency package set changed'
    differences = []
    for key, package in before_packages.items():
        revised = after_packages[key]
        if key == PACKAGE:
            assert package['source'].startswith('registry+') and 'checksum' in package
            assert revised == {k: v for k, v in package.items() if k not in ('source', 'checksum')}
            differences.append({'name': key[0], 'version': key[1],
                                'removed_source': package['source'],
                                'removed_checksum': package['checksum']})
        else:
            assert package == revised, f'unexpected locked dependency change: {key}'
    assert {k: v for k, v in before.items() if k != 'package'} == {
        k: v for k, v in after.items() if k != 'package'}
    assert len(differences) == 1
    write_json(evidence_path, differences)


def cargo_executable(stdout_path, name, test):
    found = set()
    for line in stdout_path.read_text().splitlines():
        if not line.startswith('{'):
            continue
        message = json.loads(line)
        if (message.get('reason') == 'compiler-artifact'
                and message.get('target', {}).get('name') == name
                and message.get('profile', {}).get('test') == test
                and message.get('executable')):
            found.add(message['executable'])
    assert len(found) == 1, f'expected one {name} executable in {stdout_path}: {found}'
    executable = Path(found.pop()).resolve(strict=True)
    assert executable.is_file()
    return executable


def build_variant(label, allocator, work, candidate, original_lock, commands, build_env, runtime_env):
    name = f'{allocator}-{label}'
    root = work / 'probe-workspaces' / name
    root.mkdir(parents=True)
    for relative in SOURCE_FILES:
        target = root / relative
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(HERE / relative, target)
    shutil.copyfile(original_lock, root / 'Cargo.lock')
    if (allocator, label) == ('system', 'baseline'):
        # Fetch the entire locked graph, including optional mimalloc metadata,
        # before any candidate update is required to resolve fully offline.
        commands.run(f'{name}-fetch', ['cargo', 'fetch', '--locked'],
                     cwd=root, env=build_env, timeout=1800)
    if label == 'candidate':
        config = root / '.cargo'
        config.mkdir()
        (config / 'config.toml').write_text(
            '[patch.crates-io]\nfsqlite = { path = ' + json.dumps(str(candidate)) + ' }\n')
        commands.run(f'{name}-lock-update', ['cargo', 'update', '--offline', '-p', 'fsqlite@0.4.13'],
                     cwd=root, env=build_env)
        assert_lock_delta(original_lock, root / 'Cargo.lock',
                          commands.evidence / f'{name}-lock-delta.json')
    features = ['--features', 'production-allocator'] if allocator == 'mimalloc' else []
    build = commands.run(f'{name}-build', ['cargo', 'build', '--locked', '--bins',
                                         '--message-format=json', *features],
                         cwd=root, env=build_env, timeout=7200)
    artifacts = {}
    for binary in ['memory-probe', 'future-sizes']:
        source = cargo_executable(Path(build['stdout']), binary, False)
        saved = commands.evidence / 'bin' / f'{name}-{binary}'
        shutil.copy2(source, saved)
        artifacts[binary] = {'path': str(saved), 'sha256': sha256(saved)}
    test_build = commands.run(f'{name}-preservation-build',
                              ['cargo', 'test', '--locked', '--no-run', '--test', 'preservation',
                               '--message-format=json', *features],
                              cwd=root, env=build_env, timeout=3600)
    source = cargo_executable(Path(test_build['stdout']), 'preservation', True)
    saved = commands.evidence / 'bin' / f'{name}-preservation'
    shutil.copy2(source, saved)
    artifacts['preservation'] = {'path': str(saved), 'sha256': sha256(saved)}
    shutil.copyfile(root / 'Cargo.lock', commands.evidence / f'{name}-Cargo.lock')
    if label == 'baseline':
        assert sha256(root / 'Cargo.lock') == sha256(original_lock)
    else:
        assert_lock_delta(original_lock, root / 'Cargo.lock',
                          commands.evidence / f'{name}-lock-delta.json')
    commands.run(f'{name}-future-sizes', [artifacts['future-sizes']['path']], env=runtime_env)
    return artifacts


def memory_attribution(log):
    maps = []
    workers = []
    caches = {}
    rollup = {}
    expected_workers = None
    for line in log.read_text().splitlines():
        values = {key: int(value) for key, value in re.findall(r'(\w+)=(\d+)(?:\s|$)', line)}
        if line.startswith('CONFIG '):
            expected_workers = 0 if 'backend=Raw ' in line else values['connections']
        elif line.startswith('MAP stage=measured '):
            address = re.search(r' ([0-9a-f]+)-([0-9a-f]+) ', line)
            assert address is not None
            maps.append({'start': int(address[1], 16), 'end': int(address[2], 16),
                         'header': line[address.start() + 1:], **values})
        elif line.startswith('WORKER_STACK stage=measured '):
            pointer = re.search(r' stack_pointer=(\S+)', line)
            workers.append({'tid': values.get('tid'),
                            'stack_pointer': int(pointer[1], 0) if pointer else None})
        elif line.startswith('CACHE '):
            connection = values.pop('connection')
            caches.setdefault(str(connection), {}).update(values)
            if 'unavailable=true' in line:
                caches[str(connection)]['unavailable'] = True
        elif line.startswith('ROLLUP stage=measured '):
            rollup = values
    unique_stacks = {}
    for worker in workers:
        pointer = worker['stack_pointer']
        matches = [mapping for mapping in maps
                   if pointer is not None and mapping['start'] <= pointer < mapping['end']]
        assert len(matches) <= 1
        worker['mapping'] = matches[0] if matches else None
        if matches:
            unique_stacks[matches[0]['start']] = matches[0]
    mapped = sum(w['mapping'] is not None for w in workers)
    cached = [cache['cached_pages'] for cache in caches.values() if 'cached_pages' in cache]
    return {'workers': workers, 'worker_stack_rss_kib': sum(m['rss_kib'] for m in unique_stacks.values()),
            'workers_with_mapping': mapped, 'expected_workers': expected_workers,
            'worker_stack_rss_is_complete': expected_workers is not None and mapped == expected_workers,
            'database_family_mapping_rss_kib': sum(m['rss_kib'] for m in maps if '.sqlite3' in m['header']),
            'cache_by_connection': caches,
            'cached_pages_total': sum(cached) if cached else None,
            'cache_connections_with_page_counts': len(cached),
            'smaps_rollup': rollup}


def run_matrix(name, allocator, binaries, labels, seeds, work, commands, runtime_env,
               rounds=1, backends=('adapter',), primed=False):
    output = commands.evidence / 'matrices' / name
    command = [sys.executable, '-I', HERE / 'run_matrix.py', '--rounds', str(rounds),
               '--output', output, '--fixtures-dir', work / 'fixtures' / name]
    for label in ['baseline', 'candidate']:
        command += ['--binary', f"{label}={binaries[(allocator, label)]['memory-probe']['path']}"]
    for label in labels:
        command += ['--seed', f'{label}={seeds / (label + ".sqlite3")}']
    for backend in backends:
        command += ['--backend', backend]
    if primed:
        command.append('--primed')
    run = commands.run(name, command, env=runtime_env, timeout=3600, required=False)
    results_path = output / 'results.json'
    rows = json.loads(results_path.read_text()) if results_path.exists() else []
    for row in rows:
        row['allocator'] = allocator
        row['experiment'] = name
        row['attribution'] = memory_attribution(Path(row['log']))
    complete = len(rows) == 2 * len(labels) * 2 * rounds * len(backends)
    return rows, run['success'] and complete


def print_table(rows):
    grouped = collections.defaultdict(list)
    for row in rows:
        key = (row['experiment'], row['label'], row['seed'], row['backend'], row['connections'])
        grouped[key].append(row)
    print('\nexperiment | variant | fixture | backend | conns | delta KiB range | observed stack RSS KiB | cached pages | gate')
    for key, values in grouped.items():
        deltas = [row['measured']['delta_kib'] for row in values]
        stacks = [row['attribution']['worker_stack_rss_kib'] for row in values]
        incomplete = '*' if any(not row['attribution']['worker_stack_rss_is_complete']
                                 for row in values) else ''
        caches = [row['attribution']['cached_pages_total'] for row in values
                  if row['attribution']['cached_pages_total'] is not None]
        cache_range = f'{min(caches)}..{max(caches)}' if caches else 'unavailable'
        outcome = 'PASS' if all(row['within_32_mib_budget'] for row in values) else 'FAIL'
        print(' | '.join(map(str, [*key, f'{min(deltas)}..{max(deltas)}',
                                    f'{min(stacks)}..{max(stacks)}{incomplete}', cache_range, outcome])))
    print('* Stack RSS is a partial observation when not every worker SP could be mapped; '
          'database-family mapping totals in JSON include DB/WAL/SHM files.')


def qualify(args, work, evidence, summary):
    provenance = json.loads(args.provenance.read_text())
    assert (provenance['package'], provenance['version']) == PACKAGE
    assert sha256(args.patch) == provenance['patch_sha256']
    for path in [args.patch, args.provenance]:
        shutil.copyfile(path, evidence / path.name)
    build_env = os.environ.copy()
    toolchain = tomllib.loads((args.repo / 'rust-toolchain.toml').read_text())['toolchain']['channel']
    assert build_env.get('RUSTUP_TOOLCHAIN') == toolchain, 'pin toolchain for workspaces outside the repo'
    assert build_env.get('CARGO_TARGET_DIR'), 'workflow must supply a dedicated CARGO_TARGET_DIR'
    assert Path(build_env['CARGO_TARGET_DIR']).is_absolute()
    assert build_env.get('CARGO_PROFILE_DEV_OPT_LEVEL', '1') == '1'
    assert build_env.get('CARGO_PROFILE_DEV_DEBUG') == '0'
    assert build_env.get('CARGO_INCREMENTAL') == '0'
    assert build_env.get('CARGO_PROFILE_TEST_OPT_LEVEL', '1') == '1'
    assert build_env.get('CARGO_PROFILE_TEST_DEBUG', '0') == '0'
    assert not any('ASUPERSYNC' in key and key.startswith('CARGO_PROFILE_') for key in build_env)
    manifest = tomllib.loads((HERE / 'Cargo.toml').read_text())
    assert manifest['profile']['dev']['opt-level'] == 1
    assert 'package' not in manifest['profile']['dev'], 'package profile overrides are forbidden'
    flags = build_env.get('RUSTFLAGS', '') + build_env.get('CARGO_ENCODED_RUSTFLAGS', '')
    assert 'codegen-units=256' not in flags and 'opt-level=0' not in flags
    runtime_env = build_env.copy()
    for key in ['MALLOC_CONF', '_RJEM_MALLOC_CONF', 'MALLOC_ARENA_MAX']:
        runtime_env.pop(key, None)
    runtime_env['TMPDIR'] = str(work / 'fixtures' / 'preservation')
    Path(runtime_env['TMPDIR']).mkdir(parents=True)
    write_json(evidence / 'environment.json', {
        'platform': platform.platform(), 'python': sys.version, 'cpu_count': os.cpu_count(),
        'compiler_settings': {key: build_env.get(key) for key in
                              ['RUSTUP_TOOLCHAIN', 'CARGO_TARGET_DIR', 'CARGO_BUILD_JOBS', 'CARGO_PROFILE_DEV_OPT_LEVEL',
                               'CARGO_PROFILE_DEV_DEBUG', 'CARGO_INCREMENTAL', 'RUSTFLAGS',
                               'CARGO_ENCODED_RUSTFLAGS', 'CARGO_PROFILE_TEST_OPT_LEVEL',
                               'CARGO_PROFILE_TEST_DEBUG']},
        'runtime_removed': ['MALLOC_CONF', '_RJEM_MALLOC_CONF', 'MALLOC_ARENA_MAX'],
        'source_hashes': {relative: sha256(HERE / relative) for relative in SOURCE_FILES},
    })
    commands = Commands(evidence)
    commands.run('rustc-version', ['rustc', '-Vv'], env=build_env)
    commands.run('cargo-version', ['cargo', '-Vv'], env=build_env)
    commands.run('application-revision', ['git', '-C', args.repo, 'rev-parse', 'HEAD'], env=build_env)
    archives = work / 'archives'
    archives.mkdir()
    sources = work / 'sources'
    sources.mkdir()
    archive = download(provenance['published_archive_url'], provenance['published_archive_sha256'],
                       archives / 'fsqlite-0.4.13.crate')
    published = extract_archive(archive, sources / 'published', prefix='fsqlite-0.4.13')
    assert sha256(published / provenance['changed_file']) == provenance['published_file_sha256']
    candidate = sources / 'candidate'
    shutil.copytree(published, candidate)
    commands.run('candidate-patch', ['patch', '-p1', '--batch', '--fuzz=0', '-i', args.patch],
                 cwd=candidate, env=build_env)
    assert sha256(candidate / provenance['changed_file']) == provenance['candidate_file_sha256']
    before, after = tree_hashes(published), tree_hashes(candidate)
    assert before.keys() == after.keys()
    assert [key for key in before if before[key] != after[key]] == [provenance['changed_file']]
    write_json(evidence / 'candidate-source-verification.json', {'published': before, 'candidate': after})
    old = provenance['old_cli']
    old_archive = download(old['archive_url'], old['archive_sha256'], archives / 'fsqlite-0.4.10.tar.gz')
    old_cli = extract_archive(old_archive, sources / 'old-cli', only_member=old['member'])
    assert old_cli.stat().st_size == old['executable_bytes']
    assert sha256(old_cli) == old['executable_sha256']
    old_cli.chmod(0o755)
    seeds = work / 'fixtures' / 'am-gh372-memory-seeds'
    commands.run('prepare-fixtures', [sys.executable, '-I', HERE / 'prepare_fixtures.py',
                                     '--repo', args.repo, '--old-cli', old_cli, '--output', seeds],
                 env=runtime_env, timeout=1200)
    for name in ['fixture-verification.json', 'old-cli.stdout', 'old-cli.stderr', 'metrics-insert-0.4.10.sql']:
        shutil.copyfile(seeds / name, evidence / name)
    original_lock = evidence / 'original-Cargo.lock'
    shutil.copyfile(HERE / 'Cargo.lock', original_lock)
    facade = [package for package in tomllib.loads(original_lock.read_text())['package']
              if (package['name'], package['version']) == PACKAGE]
    assert len(facade) == 1 and facade[0]['checksum'] == provenance['published_archive_sha256']
    binaries = {}
    errors = summary['errors']
    for allocator in ['system', 'mimalloc']:
        for label in ['baseline', 'candidate']:
            try:
                binaries[(allocator, label)] = build_variant(
                    label, allocator, work, candidate, original_lock, commands, build_env, runtime_env)
            except Exception as error:
                errors.append(f'build {allocator}-{label}: {error}')
            write_json(evidence / 'binaries.json', {
                f'{allocator}-{label}': value for (allocator, label), value in binaries.items()})
    rows = summary['measurements']
    for allocator in ['system', 'mimalloc']:
        if not all((allocator, label) in binaries for label in ['baseline', 'candidate']):
            continue
        labels = ['tiny', 'healthy', 'double', 'sparse', 'dense'] if allocator == 'system' else [
            'healthy', 'sparse', 'dense']
        experiments = [(f'{allocator}-main', labels, 3, ('adapter',), False, runtime_env)]
        if allocator == 'system':
            arena_env = runtime_env.copy()
            arena_env['MALLOC_ARENA_MAX'] = '1'
            experiments += [
                ('system-controls', ['healthy'], 1, ('raw', 'async'), False, runtime_env),
                ('system-primed', ['healthy', 'sparse', 'dense'], 1, ('adapter',), True, runtime_env),
                ('system-arena1', ['healthy'], 1, ('adapter',), False, arena_env),
            ]
        for name, labels, rounds, backends, primed, env in experiments:
            try:
                results, passed = run_matrix(name, allocator, binaries, labels, seeds, work,
                                             commands, env, rounds, backends, primed)
                rows.extend(results)
                summary['matrix_correctness'].append({'name': name, 'passed': passed})
                if not passed:
                    errors.append(f'{name}: execution/content verification incomplete or failed')
            except Exception as error:
                summary['matrix_correctness'].append({'name': name, 'passed': False})
                errors.append(f'{name}: {error}')
            write_json(evidence / 'measurements.json', rows)
    for (allocator, label), artifacts in binaries.items():
        name = f'{allocator}-{label}-preservation'
        result = commands.run(name, [artifacts['preservation']['path'], '--nocapture', '--test-threads=1'],
                              env=runtime_env, timeout=600, required=False)
        text = Path(result['stdout']).read_text() + Path(result['stderr']).read_text()
        verdicts = re.findall(r'^test result: ok\. 2 passed; 0 failed; 1 ignored; '
                              r'0 measured; 0 filtered out; finished in .+$', text, flags=re.M)
        passed = result['success'] and len(verdicts) == 1
        summary['preservation'].append({'allocator': allocator, 'label': label,
                                        'passed': passed, 'command': result})
        if not passed:
            errors.append(f'{name}: expected exactly two passed tests and one ignored child')
    main_rows = [row for row in rows if row['experiment'].endswith('-main')]
    baseline = [row for row in main_rows if row['allocator'] == 'system'
                and row['label'] == 'baseline' and row['seed'] == 'healthy' and row['connections'] == 5]
    candidates = [row for row in main_rows if row['label'] == 'candidate']
    summary['native_baseline_reproduced'] = len(baseline) == 3 and any(
        row['measured']['delta_kib'] > BUDGET_KIB for row in baseline)
    summary['baseline_failing_repetitions'] = sum(
        row['measured']['delta_kib'] > BUDGET_KIB for row in baseline)
    summary['candidate_32_mib_passed'] = len(candidates) == 48 and all(
        row['within_32_mib_budget'] for row in candidates)
    summary['preserves'] = (len(summary['preservation']) == 4
                            and all(result['passed'] for result in summary['preservation'])
                            and len(summary['matrix_correctness']) == 5
                            and all(result['passed'] for result in summary['matrix_correctness']))
    # A passing candidate without a reproducing baseline is inconclusive, not
    # verified evidence that this source change corrected the reported cost.
    summary['success'] = (summary['native_baseline_reproduced']
                          and summary['candidate_32_mib_passed']
                          and summary['preserves'] and not errors)
    print_table(rows)


def main():
    if not __debug__:
        raise RuntimeError('use python3 -I; verification assertions must remain enabled')
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--repo', type=Path, required=True)
    parser.add_argument('--work-dir', type=Path, required=True)
    parser.add_argument('--patch', type=Path, required=True)
    parser.add_argument('--provenance', type=Path, required=True)
    args = parser.parse_args()
    for name in ['repo', 'patch', 'provenance']:
        setattr(args, name, getattr(args, name).resolve(strict=True))
    work = owned_directory(args.work_dir)
    evidence = work / 'evidence'
    evidence.mkdir()
    (evidence / 'bin').mkdir()
    (evidence / 'matrices').mkdir()
    summary = {'success': False, 'native_baseline_reproduced': False,
               'candidate_32_mib_passed': False, 'preserves': False,
               'budget_kib': BUDGET_KIB, 'errors': [], 'measurements': [], 'preservation': [],
               'matrix_correctness': [],
               'method': 'Standalone SQLModel diagnostic; VmHWM minus baseline VmRSS; no peak reset.',
               'application_integration_test_executed': False,
               'pending': ['actual repository memory regression', 'full workspace checks/Clippy',
                           'other platforms', 'release profile and swarm-soak qualification']}
    try:
        qualify(args, work, evidence, summary)
    except Exception as error:
        summary['errors'].append(f'setup/orchestration failure: {type(error).__name__}: {error}')
    finally:
        write_json(evidence / 'summary.json', summary)
        print(json.dumps({key: summary[key] for key in ['success', 'native_baseline_reproduced',
                                                       'candidate_32_mib_passed', 'preserves', 'errors']},
                         indent=2), flush=True)
        print(f'EVIDENCE {evidence}', flush=True)
    return 0 if summary['success'] else 1


if __name__ == '__main__':
    raise SystemExit(main())
