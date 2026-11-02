#!/usr/bin/env python3
"""Compile exact repository libtests serially and preserve their provenance."""
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import resource
import shutil
import subprocess
import sys
import time

root = Path(os.environ['GH362_RUN_ROOT']).resolve(strict=True)
label, checkout = sys.argv[1:3]
checkout = Path(checkout)
toolchain = Path(os.environ['GH362_TOOLCHAIN_BIN']).resolve(strict=True)
env = dict(os.environ)
for variable in ['CARGO_ENCODED_RUSTFLAGS', 'CARGO_BUILD_RUSTFLAGS', 'RUSTC_WORKSPACE_WRAPPER']:
    env.pop(variable, None)
for variable in list(env):
    if variable.startswith('CARGO_PROFILE_'):
        env.pop(variable)
settings = {
    'PATH': str(toolchain) + os.pathsep + env.get('PATH', ''),
    'CARGO_HOME': str(Path(os.environ['GH362_CARGO_HOME']).resolve()),
    'CARGO_TARGET_DIR': str(root / 'mail-target'),
    'CARGO_BUILD_JOBS': '1',
    'CARGO_PROFILE_DEV_DEBUG': '0',
    'CARGO_PROFILE_DEV_OPT_LEVEL': '0',
    'CARGO_PROFILE_TEST_DEBUG': '0',
    'CARGO_PROFILE_TEST_OPT_LEVEL': '0',
    'CARGO_INCREMENTAL': '0',
    'CARGO_TERM_COLOR': 'never',
    'MALLOC_CONF': 'narenas:1,dirty_decay_ms:0,muzzy_decay_ms:0,background_thread:false,tcache:false',
    'RUSTFLAGS': '-Zthreads=1',
    'RUSTC': str(toolchain / 'rustc'),
    'RUSTC_WRAPPER': str(Path(__file__).with_name('gh362_rustc_wrapper.py').resolve()),
}
env.update(settings)
command = [str(toolchain / 'cargo'), 'test', '--locked',
    '-p', 'mcp-agent-mail-storage', '-p', 'mcp-agent-mail-core',
    '--no-default-features', '--features', 'mcp-agent-mail-storage/portable',
    '--lib', '--no-run', '--config', 'net.git-fetch-with-cli=true']
def sha256(path):
    digest = hashlib.sha256()
    with path.open('rb') as f:
        for chunk in iter(lambda: f.read(1024 * 1024), b''):
            digest.update(chunk)
    return digest.hexdigest()

def read_optional(path):
    try:
        return Path(path).read_text().strip()
    except OSError:
        return None

source_names = ['Cargo.lock', 'Cargo.toml',
    'crates/mcp-agent-mail-storage/Cargo.toml',
    'crates/mcp-agent-mail-core/Cargo.toml',
    'crates/mcp-agent-mail-core/src/lib.rs',
    'crates/mcp-agent-mail-core/src/reservation_artifact.rs',
    'crates/mcp-agent-mail-storage/src/lib.rs',
    'crates/mcp-agent-mail-storage/src/canonical_watch.rs',
    'crates/mcp-agent-mail-storage/src/canonical_id_tests.rs']
manifest = {
    'label': label, 'checkout': str(checkout), 'command': command,
    'environment': settings, 'profile': 'unoptimized dev/test; opt-level=0; debuginfo=0; incremental=false; Asupersync only uses MIR optimization level 0, fewer LLVM-IR names, and hint-mostly-unused to reduce compiler memory and defer unused code generation',
    'compiler': subprocess.check_output([str(toolchain / 'rustc'), '-vV'], env=env, text=True),
    'base_revision': subprocess.check_output(['git', 'rev-parse', 'HEAD'], cwd=checkout, text=True).strip(),
    'started_utc': time.strftime('%Y-%m-%dT%H:%M:%SZ', time.gmtime()),
    'source_sha256_before': {name: sha256(checkout / name) for name in source_names if (checkout / name).exists()},
    'host': {'uname': platform.uname()._asdict(),
        'cgroup_memory_max': read_optional('/sys/fs/cgroup/memory.max'),
        'cgroup_cpu_max': read_optional('/sys/fs/cgroup/cpu.max')},
    'cgroup_memory_events_before': read_optional('/sys/fs/cgroup/memory.events'),
    'compiler_wrapper': {'path': settings['RUSTC_WRAPPER'],
        'sha256': sha256(Path(settings['RUSTC_WRAPPER'])),
        'behavior': 'Appends -Zunstable-options --jobs-backend=1 --jobs-linker=1 only to actual crate compilation; additionally appends -Zfewer-names=yes -Zmir-opt-level=0 -Zhint-mostly-unused=yes -Ztime-passes only for Asupersync. Version and target probes pass through unchanged. No source, feature, dependency fingerprint, or artifact modification; the Asupersync MIR optimization adjustment and deferred code-generation hint are explicit compiler memory tradeoffs shared by both benchmark arms. time-passes only logs compiler phase timing and memory.'},
}
if label.startswith('baseline'):
    manifest['incumbent_harness_notes'] = 'Production algorithms unchanged. Added cfg(test) timing counters and shared public-writer benchmark module. Added dev-dependency Nix features fs,inotify to align the candidate libtest dependency feature graph.'
manifest_path = root / (label + '-libtests-build.json')
log_path = root / (label + '-libtests-build.log')
if manifest_path.exists() or log_path.exists():
    raise SystemExit('Refusing to overwrite a prior build attempt; use a new run root or label')
manifest['log'] = str(log_path)
manifest_path.write_text(json.dumps(manifest, indent=2) + '\n')
print(json.dumps({'log': str(log_path), 'manifest': str(manifest_path), 'command': command}), flush=True)
started = time.monotonic()
if label.startswith('candidate-hint'):
    check_env = dict(env)
    check_env.pop('RUSTC_WRAPPER', None)
    check_command = [str(toolchain / 'cargo'), 'check', '--locked',
        '-p', 'mcp-agent-mail-storage', '--no-default-features',
        '--features', 'portable', '--lib', '--config', 'net.git-fetch-with-cli=true']
    check_log_path = root / (label + '-production-check.log')
    check_started = time.monotonic()
    with check_log_path.open('w') as check_log:
        check_result = subprocess.run(check_command, cwd=checkout, env=check_env, stdout=check_log, stderr=subprocess.STDOUT)
    manifest['production_recheck'] = {'command': check_command,
        'rustflags': settings['RUSTFLAGS'], 'compiler_wrapper': None,
        'log': str(check_log_path), 'exit_code': check_result.returncode,
        'elapsed_seconds': time.monotonic() - check_started}
    manifest_path.write_text(json.dumps(manifest, indent=2) + '\n')
    print('Production recheck: ' + json.dumps(manifest['production_recheck']), flush=True)
    if check_result.returncode != 0:
        sys.exit(check_result.returncode)
with log_path.open('w') as log:
    completed = subprocess.run(command, cwd=checkout, env=env, stdout=log, stderr=subprocess.STDOUT)
manifest['elapsed_seconds'] = time.monotonic() - started
manifest['exit_code'] = completed.returncode
manifest['max_child_rss_kib'] = resource.getrusage(resource.RUSAGE_CHILDREN).ru_maxrss
manifest['finished_utc'] = time.strftime('%Y-%m-%dT%H:%M:%SZ', time.gmtime())
manifest['cgroup_memory_events_after'] = read_optional('/sys/fs/cgroup/memory.events')
manifest['source_sha256_after'] = {name: sha256(checkout / name) for name in source_names if (checkout / name).exists()}
manifest['compiler_wrapper']['sha256_after'] = sha256(Path(settings['RUSTC_WRAPPER']))
manifest['artifacts'] = []
if completed.returncode == 0:
    artifact_dir = root / 'native-binaries'
    artifact_dir.mkdir(exist_ok=True)
    for path in re.findall(r'Executable unittests .* \(([^)]+)\)', log_path.read_text()):
        original = Path(path)
        crate = original.name.rsplit('-', 1)[0]
        destination = artifact_dir / (label + '-' + crate)
        shutil.copy2(original, destination)
        manifest['artifacts'].append({'crate': crate, 'original': str(original), 'preserved': str(destination), 'sha256': sha256(destination), 'bytes': destination.stat().st_size})
    feature_command = [str(toolchain / 'cargo'), 'tree', '--locked', '--offline',
        '-p', 'mcp-agent-mail-storage', '-p', 'mcp-agent-mail-core',
        '--no-default-features', '--features', 'mcp-agent-mail-storage/portable',
        '--edges', 'features']
    feature_path = root / (label + '-libtests-features.txt')
    with feature_path.open('w') as feature_file:
        feature_result = subprocess.run(feature_command, cwd=checkout, env=env, stdout=feature_file, stderr=subprocess.STDOUT)
    manifest['feature_tree'] = {'command': feature_command, 'exit_code': feature_result.returncode, 'path': str(feature_path), 'sha256': sha256(feature_path)}
    inventory_command = [str(toolchain / 'cargo'), 'tree', '--locked', '--offline',
        '-p', 'mcp-agent-mail-storage', '-p', 'mcp-agent-mail-core',
        '--no-default-features', '--features', 'mcp-agent-mail-storage/portable',
        '--edges', 'normal,build,dev', '--prefix', 'none', '--format', '{p}|{f}']
    inventory_result = subprocess.run(inventory_command, cwd=checkout, env=env, capture_output=True, text=True)
    inventory_raw_path = root / (label + '-package-features-raw.txt')
    inventory_raw_path.write_text(inventory_result.stdout)
    inventory_stderr_path = root / (label + '-package-features-stderr.txt')
    inventory_stderr_path.write_text(inventory_result.stderr)
    rows = set()
    for line in inventory_result.stdout.splitlines():
        line = line.strip().removesuffix(' (*)').replace(str(checkout), '<CHECKOUT>')
        if not line:
            continue
        package, features = line.rsplit('|', 1)
        rows.add(package + '|' + ','.join(sorted(set(filter(None, features.split(','))))))
    inventory_path = root / (label + '-package-features.txt')
    inventory_path.write_text('\n'.join(sorted(rows)) + '\n')
    manifest['resolved_package_features'] = {'command': inventory_command,
        'exit_code': inventory_result.returncode, 'path': str(inventory_path),
        'raw_path': str(inventory_raw_path), 'stderr_path': str(inventory_stderr_path),
        'rows': len(rows), 'sha256': sha256(inventory_path)}
manifest_path.write_text(json.dumps(manifest, indent=2) + '\n')
print(json.dumps(manifest, indent=2), flush=True)
sys.exit(completed.returncode)
