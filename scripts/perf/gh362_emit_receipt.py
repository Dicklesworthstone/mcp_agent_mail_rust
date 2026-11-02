#!/usr/bin/env python3
"""Bind real native binaries and frozen source to their compiler provenance."""
import hashlib
import json
import os
from pathlib import Path
import sys

root = Path(os.environ['GH362_RUN_ROOT']).resolve(strict=True)

def sha256(path):
    digest = hashlib.sha256()
    with path.open('rb') as f:
        for chunk in iter(lambda: f.read(1024 * 1024), b''):
            digest.update(chunk)
    return digest.hexdigest()

def require(condition, details):
    if not condition:
        raise ValueError(details)

receipt = {'schema_version': 1, 'arms': {}}
for name in sys.argv[1:]:
    path = Path(name)
    manifest = json.loads(path.read_text())
    arm = 'baseline' if manifest['label'].startswith('baseline') else 'candidate'
    require(arm not in receipt['arms'], ('duplicate arm', arm))
    require(manifest.get('exit_code') == 0, (name, 'native build did not succeed'))
    require(manifest['source_sha256_before'] == manifest['source_sha256_after'], (name, 'source changed during build'))
    require(manifest['compiler_wrapper']['sha256'] == manifest['compiler_wrapper']['sha256_after'], (name, 'compiler wrapper changed during build'))
    inventory = manifest['resolved_package_features']
    require(inventory['exit_code'] == 0 and inventory['rows'] > 0, (name, 'resolved features unavailable'))
    require(sha256(Path(inventory['path'])) == inventory['sha256'], (name, 'feature inventory changed'))
    binaries = {}
    for artifact in manifest['artifacts']:
        component = {'mcp_agent_mail_storage': 'storage', 'mcp_agent_mail_core': 'core'}.get(artifact['crate'])
        if component is None:
            continue
        binary = Path(artifact['preserved'])
        require(sha256(binary) == artifact['sha256'], (name, 'binary changed', component))
        binaries[component] = {'path': str(binary), 'sha256': artifact['sha256'], 'bytes': artifact['bytes']}
    require('storage' in binaries, (name, 'missing storage native binary'))
    if arm == 'candidate':
        require('core' in binaries, (name, 'missing core native binary'))
    receipt['arms'][arm] = {
        'binaries': binaries,
        'sources': {'head': manifest['base_revision'], 'root': manifest['checkout'],
            'files_sha256_before': manifest['source_sha256_before'],
            'files_sha256_after': manifest['source_sha256_after']},
        'build': {'exit_code': manifest['exit_code'], 'compiler': manifest['compiler'],
            'profile': manifest['profile'], 'rustflags': manifest['environment']['RUSTFLAGS'],
            'wrapper_sha256': manifest['compiler_wrapper']['sha256'],
            'features': {'packages': ['mcp-agent-mail-storage', 'mcp-agent-mail-core'],
                'default_features': False, 'requested': ['mcp-agent-mail-storage/portable'],
                'resolved_package_features_sha256': inventory['sha256']},
            'source_hashes_unchanged': True, 'command': manifest['command'],
            'environment': manifest['environment'], 'compiler_wrapper': manifest['compiler_wrapper'],
            'manifest_path': str(path), 'manifest_sha256': sha256(path),
            'feature_tree': manifest['feature_tree'], 'resolved_package_features': inventory,
            'started_utc': manifest['started_utc'], 'finished_utc': manifest['finished_utc']},
    }
if len(receipt['arms']) == 2:
    candidate = receipt['arms']['candidate']['build']
    baseline = receipt['arms']['baseline']['build']
    for key in ['compiler', 'profile', 'features', 'rustflags', 'wrapper_sha256']:
        require(candidate[key] == baseline[key], ('paired compiler/feature configuration differs', key))
    destination = root / 'native-binaries' / 'paired-build-receipt.json'
else:
    destination = root / 'native-binaries' / (next(iter(receipt['arms'])) + '-build-receipt.json')
with destination.open('x') as output:
    output.write(json.dumps(receipt, indent=2) + '\n')
print(json.dumps({'receipt': str(destination), 'sha256': sha256(destination), 'arms': list(receipt['arms'])}))
