#!/usr/bin/env python3
"""Try official releases, keep only a candidate passing every acceptance gate.
No push, publication, production checkout update, or version rewriting.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tomllib
from datetime import datetime, timezone
import smoke_tuta_api

HERE = Path(__file__).resolve().parent
OFFICIAL = 'https://github.com/tutao/tutanota.git'
BRIDGE_REF = 'b375f1c275b8162ee8f04008144e3dd2c72a10d6'
MINIMUM = (359, 260904, 0)
TAG = re.compile(r'^tutanota-release-(\d+)\.(\d+)\.(\d+)$')
REQUIRED = ('patch', 'build', 'bridge_tests', 'sdk_tests', 'network', 'protocol')


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def eligible(record):
    return all(record.get('checks', {}).get(key) == 'passed' for key in REQUIRED)


def candidates(output, only=None, limit=3):
    tags = {}
    for line in output.splitlines():
        sha, ref = line.split()
        name = ref.removeprefix('refs/tags/').removesuffix('^{}')
        match = TAG.fullmatch(name)
        if not match or (only and name not in only):
            continue
        version = tuple(map(int, match.groups()))
        if version < MINIMUM:
            continue
        if name not in tags or ref.endswith('^{}'):
            tags[name] = {'tag': name, 'sha': sha, 'version': version}
    return sorted(tags.values(), key=lambda row: row['version'], reverse=True)[:limit]


def choose(rows, attempt):
    reports = []
    for row in rows:
        result = attempt(row)
        reports.append(result)
        if eligible(result):
            return result, reports
    return None, reports


def run(cmd, cwd, logfile, env=None):
    with logfile.open('ab') as stream:
        stream.write(('COMMAND: ' + ' '.join(map(str, cmd)) + '\n').encode())
        result = subprocess.run(cmd, cwd=cwd, env=env, stdout=stream, stderr=subprocess.STDOUT)
    if result.returncode:
        raise RuntimeError(f'{logfile.name}: exit {result.returncode}')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--bridge-source', required=True, help='Local bridge Git repository')
    parser.add_argument('--sdk-cache', required=True, help='Local SDK Git cache (read-only)')
    parser.add_argument('--output', type=Path, required=True, help='New run directory; must not exist')
    parser.add_argument('--target-dir', type=Path)
    parser.add_argument('--tag', action='append', help='Restrict to an official stable tag')
    parser.add_argument('--limit', type=int, default=3)
    args = parser.parse_args()
    if args.limit < 1:
        parser.error('--limit must be positive')
    args.output = args.output.resolve()
    args.output.mkdir(parents=True, exist_ok=False)
    config = json.loads((HERE/'manifest.json').read_text())
    # Validate all inputs before creating candidate workspaces.
    for name, expected in config['files'].items():
        if digest(HERE/name) != expected:
            raise RuntimeError(f'Prototype file checksum mismatch: {name}')
    catalog = subprocess.check_output(['git', 'ls-remote', '--tags', OFFICIAL, 'tutanota-release-*'], text=True)
    (args.output/'official-tags.txt').write_text(catalog)
    rows = candidates(catalog, args.tag, args.limit)
    env = os.environ.copy()
    env['CARGO_TARGET_DIR'] = str((args.target_dir or args.output/'target').resolve())

    def attempt(row):
        directory = args.output/row['tag']
        logs = directory/'logs'
        logs.mkdir(parents=True)
        bridge = directory/'bridge'
        sdk = bridge/'tuta-repo'
        result = dict(row, checks={}, status='not_started')
        stage = 'prepare'
        try:
            run(['git','clone','--shared',str(Path(args.bridge_source).resolve()),str(bridge)], HERE, logs/'prepare.log')
            run(['git','checkout','--detach',BRIDGE_REF],bridge,logs/'prepare.log')
            run(['git','apply',str(HERE/'bridge.patch')],bridge,logs/'prepare.log')
            run(['git','clone','--shared','--no-checkout',str(Path(args.sdk_cache).resolve()),str(sdk)],HERE,logs/'prepare.log')
            run(['git','sparse-checkout','set','tuta-sdk/rust','test','src/app-kit/mimimi'],sdk,logs/'prepare.log')
            # Fetch the chosen tag from official upstream, never rely on fork tag names.
            run(['git','fetch','--depth=1',OFFICIAL,'refs/tags/'+row['tag']],sdk,logs/'prepare.log')
            actual = subprocess.check_output(['git','rev-parse','FETCH_HEAD^{commit}'],cwd=sdk,text=True).strip()
            if actual != row['sha']:
                raise RuntimeError('Official tag changed since discovery')
            run(['git','checkout','--detach',actual],sdk,logs/'prepare.log')
            stage = 'patch'
            for name in config['patches']:
                run(['git','apply','--check',str(HERE/name)],sdk,logs/'patch.log')
                run(['git','apply',str(HERE/name)],sdk,logs/'patch.log')
            version = tomllib.loads((sdk/'Cargo.toml').read_text())['workspace']['package']['version']
            if tuple(map(int, version.split('.'))) != tuple(row['version']):
                raise RuntimeError('Version differs from official tag; refusing a rewritten version')
            # These high-conflict SDK interfaces must remain byte-for-byte upstream.
            for path in ['tuta-sdk/rust/sdk/src/mail_facade.rs','tuta-sdk/rust/sdk/src/folder_system.rs','Cargo.toml']:
                original = subprocess.check_output(['git','show',f'{actual}:{path}'],cwd=sdk)
                if (sdk/path).read_bytes() != original:
                    raise RuntimeError(f'Unexpected change to protected upstream file: {path}')
            result['checks'][stage] = 'passed'
            commands = {
                'build': ['cargo','build','-p','tutabridge'],
                'bridge_tests': ['cargo','test','--locked','-p','tutabridge','-p','tutabridge-core','-p','tutabridge-tuta'],
                'sdk_tests': ['cargo','test','--manifest-path','tuta-repo/Cargo.toml','-p','tuta-sdk','--lib'],
            }
            for stage, command in commands.items():
                run(command,bridge,logs/f'{stage}.log',env)
                result['checks'][stage] = 'passed'
            stage = 'network'
            run([sys.executable, str(HERE/'network_probe.py'), str(bridge)],HERE,logs/'network.log')
            result['checks'][stage] = 'passed'
            stage = 'protocol'
            run(['cargo','test','--locked','-p','tutabridge-tuta','--test','mail_extensions','protocol_','--','--ignored'],bridge,logs/'protocol.log',env)
            if 'running 2 tests' not in (logs/'protocol.log').read_text():
                raise RuntimeError('Both required protocol tests must execute')
            result['checks'][stage] = 'passed'
            result['status'] = 'candidate_for_review'
            result['cargo_lock_sha256'] = digest(bridge/'Cargo.lock')
        except (RuntimeError, OSError, subprocess.CalledProcessError) as error:
            result['checks'][stage] = 'failed'
            result['status'] = stage+'_failed'
            result['error'] = str(error)
        (directory/'result.json').write_text(json.dumps(result, indent=2))
        print(f"{row['tag']}: {result['status']}", flush=True)
        return result

    selected, reports = choose(rows, attempt)
    report = {'timestamp': datetime.now(timezone.utc).isoformat(), 'bridge_ref': BRIDGE_REF,
              'minimum': MINIMUM, 'selected': selected, 'candidates': reports,
              'prototype_manifest_sha256': digest(HERE/'manifest.json')}
    (args.output/'report.json').write_text(json.dumps(report,indent=2))
    if selected:
        (args.output/'candidate.lock.json').write_text(json.dumps(report,indent=2))
    return 0 if selected else 1

if __name__ == '__main__':
    sys.exit(main())
