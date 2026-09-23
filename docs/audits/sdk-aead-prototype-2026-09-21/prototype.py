#!/usr/bin/env python3
"""Try official releases and propose candidates without regressions against a measured baseline.
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
PROTOCOL_CASES = {
    'aead_v2': ('aead_v2_mail_without_session_key_loads_through_sdk_and_inline', None),
    'optional_empty': ('protocol_accepts_optional_empty_body_text', None),
    'aead_v3': ('protocol_accepts_valid_aead_v3_mail_subject',
                'valid AEAD ciphertext must be accepted by entity decoder: InternalSdkError { error_message: "MacError" }'),
}


def protocol_outcome(name, returncode, output):
    """A missing test, compilation error or unrelated panic is never a known limitation."""
    test, known_failure = PROTOCOL_CASES[name]
    if 'running 1 test' not in output:
        return 'error'
    if returncode == 0 and f'test {test} ... ok' in output and '1 passed; 0 failed' in output:
        return 'supported'
    if (returncode == 101 and known_failure and known_failure in output
            and f'test {test} ... FAILED' in output and '0 passed; 1 failed' in output):
        return 'unsupported'
    return 'error'


def assess_capabilities(baseline, current, required=()):
    known_states = {'supported', 'unsupported'}
    errors = [name for name in PROTOCOL_CASES
              if (baseline.get(name) not in known_states | {'not_assessed'}
                  or current.get(name) not in known_states
                  or (baseline.get(name) == 'not_assessed' and current.get(name) != 'supported'))]
    regressions = [name for name in PROTOCOL_CASES
                   if baseline.get(name) == 'supported' and current.get(name) == 'unsupported']
    missing_required = [name for name in required if current.get(name) != 'supported']
    return {
        'passed': not (errors or regressions or missing_required),
        'errors': errors,
        'regressions': regressions,
        'missing_required': missing_required,
        'known_limitations': [name for name in PROTOCOL_CASES
                              if baseline.get(name) == current.get(name) == 'unsupported'],
        'newly_verified': [name for name in PROTOCOL_CASES
                           if baseline.get(name) == 'not_assessed' and current.get(name) == 'supported'],
        'improvements': [name for name in PROTOCOL_CASES
                         if baseline.get(name) == 'unsupported' and current.get(name) == 'supported'],
    }



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
    parser.add_argument('--baseline', type=Path, default=HERE/'capability-baseline.json',
                        help='Measured reference capabilities; default is the preceding 359 prototype, not a production rollout claim')
    parser.add_argument('--require-capability', action='append', choices=PROTOCOL_CASES, default=[],
                        help='Require this capability even if absent from the baseline')
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
    baseline = json.loads(args.baseline.read_text())
    if not baseline.get('reference') or not baseline.get('evidence'):
        raise RuntimeError('Baseline must identify its reference and test evidence')
    for item in baseline['evidence']:
        if digest(args.baseline.parent/item['file']) != item['sha256']:
            raise RuntimeError('Baseline evidence checksum mismatch')
    if any(baseline.get('capabilities', {}).get(name) not in {'supported', 'unsupported', 'not_assessed'} for name in PROTOCOL_CASES):
        raise RuntimeError('Baseline has missing or inconclusive protocol checks')
    catalog = subprocess.check_output(['git', 'ls-remote', '--tags', OFFICIAL, 'tutanota-release-*'], text=True)
    (args.output/'official-tags.txt').write_text(catalog)
    rows = candidates(catalog, args.tag, args.limit)
    env = os.environ.copy()
    target_root = (args.target_dir or args.output/'target').resolve()
    env['CARGO_INCREMENTAL'] = '0'

    def attempt(row):
        directory = args.output/row['tag']
        logs = directory/'logs'
        logs.mkdir(parents=True)
        bridge = directory/'bridge'
        sdk = bridge/'tuta-repo'
        result = dict(row, checks={}, status='not_started')
        bridge_env = dict(env, CARGO_TARGET_DIR=str(target_root/row['tag']/'bridge'))
        sdk_env = dict(env, CARGO_TARGET_DIR=str(target_root/row['tag']/'sdk'))
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
                'sdk_tests': ['cargo','test','--manifest-path','tuta-repo/Cargo.toml','-p','tuta-sdk','--lib','--test','interactive_session_test'],
            }
            for stage, command in commands.items():
                run(command,bridge,logs/f'{stage}.log',sdk_env if stage == 'sdk_tests' else bridge_env)
                result['checks'][stage] = 'passed'
            stage = 'network'
            run([sys.executable, str(HERE/'network_probe.py'), str(bridge)],HERE,logs/'network.log')
            result['checks'][stage] = 'passed'
            stage = 'protocol'
            outcomes = {}
            for name, (test_name, _) in PROTOCOL_CASES.items():
                command = ['cargo', 'test', '--locked', '-p', 'tutabridge-tuta', '--test',
                           'mail_extensions', test_name, '--', '--include-ignored', '--exact']
                completed = subprocess.run(command, cwd=bridge, env=bridge_env,
                                           stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
                output = completed.stdout
                (logs/f'protocol-{name}.log').write_text(output)
                outcomes[name] = protocol_outcome(name, completed.returncode, output)
            assessment = assess_capabilities(baseline['capabilities'], outcomes, args.require_capability)
            result['capabilities'] = outcomes
            result['capability_comparison'] = assessment
            result['baseline_reference'] = baseline['reference']
            result['baseline_sha256'] = digest(args.baseline)
            result['automatic_promotion_allowed'] = False
            if not assessment['passed']:
                raise RuntimeError('Protocol regression, inconclusive check, or required capability missing')
            result['checks'][stage] = 'passed'
            result['status'] = ('candidate_for_review_with_known_limitations'
                                if assessment['known_limitations'] else 'candidate_for_review')
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
