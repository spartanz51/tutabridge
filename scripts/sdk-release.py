#!/usr/bin/env python3
"""Git-only SDK release coordination; upstream code runs in separate CI jobs."""
import argparse
import base64
import gzip
import hashlib
import io
import importlib.util
from datetime import date, datetime, timezone
import json
import os
from pathlib import Path
import re
import subprocess
import tomllib

REPO = 'spartanz51/tutabridge'
UPSTREAM = 'https://github.com/tutao/tutanota.git'
BOT_NAME = 'github-actions[bot]'
BOT_EMAIL = '41898282+github-actions[bot]@users.noreply.github.com'
LEGACY_ALLOWED = {'.gitmodules', 'sdk/BASE', 'tuta-repo', 'release-plan.json', 'Cargo.lock'}
ALLOWED = {'sdk/BASE', 'sdk/EXPECTED', 'tuta-repo', 'release-plan.json', 'Cargo.lock'}
LOCK_ENCODED_LIMIT = 64 * 1024
LOCK_DIFF_LIMIT = 1024 * 1024
TAG = re.compile(r'tutanota-release-(\d+)\.(\d{6})\.(\d+)\Z')
VERSION = re.compile(r'0\.1\.0-rc\.(\d+)\Z')
SHA = re.compile(r'[0-9a-f]{40}\Z')


def run(*args, **kwargs):
    return subprocess.check_output(args, text=True, **kwargs).strip()


def git(*args):
    return run('git', *args)


def api(path, method='GET', data=None):
    args = ['gh', 'api', f'repos/{REPO}/{path}']
    if method != 'GET':
        args += ['--method', method]
    if data is not None:
        args += ['--input', '-']
    out = run(*args, input=json.dumps(data) if data is not None else None)
    return json.loads(out) if out else None


def releases():
    return json.loads(run('gh', 'api', '--paginate', '--slurp', f'repos/{REPO}/releases?per_page=100'))


def release_list():
    return [r for page in releases() for r in page]


def outputs(**values):
    with open(os.environ['GITHUB_OUTPUT'], 'a') as stream:
        for key, value in values.items():
            value = str(value).lower() if isinstance(value, bool) else str(value)
            if '\n' in value or '\r' in value:
                raise ValueError('Multiline workflow output')
            stream.write(f'{key}={value}\n')


def tag_key(tag):
    match = TAG.fullmatch(tag)
    if not match:
        raise ValueError(f'Invalid official release tag: {tag}')
    return tuple(map(int, match.groups()))


def scheduled_today(today):
    return (today - date(2026, 10, 19)).days % 14 == 0


def base_at(ref):
    return dict(line.split('=', 1) for line in git('show', f'{ref}:sdk/BASE').splitlines() if '=' in line and not line.startswith('#'))


def next_version(tags, drafts):
    numbers = []
    for tag in set(tags) | set(drafts):
        match = VERSION.fullmatch(tag.removeprefix('v'))
        if match:
            numbers.append(int(match[1]))
        elif re.fullmatch(r'v?\d+\.\d+\.\d+(?:-rc\.\d+)?', tag):
            raise ValueError(f'Release series needs an explicit policy change: {tag}')
    if not numbers:
        raise ValueError('No existing rc release series')
    return f'0.1.0-rc.{max(numbers) + 1}'


def lock_bytes(ref):
    entry = git('ls-tree', ref, '--', 'Cargo.lock').split()
    if not entry or entry[0] != '100644':
        raise ValueError('Cargo.lock must be a regular non-executable file')
    return subprocess.check_output(['git', 'show', f'{ref}:Cargo.lock'])


def validate_lock(before, after):
    old, new = (tomllib.loads(data.decode('utf-8')) for data in (before, after))
    if not isinstance(new.get('version'), int) or not isinstance(new.get('package'), list):
        raise ValueError('Invalid Cargo.lock structure')
    sources = {p.get('source') for p in old['package']}
    sources.add('registry+https://github.com/rust-lang/crates.io-index')
    sources.add(None)
    if any(p.get('source') not in sources for p in new['package']) or old.get('patch') != new.get('patch'):
        raise ValueError('New dependency source or patch section requires review')
    return hashlib.sha256(after).hexdigest()


def lock(args):
    before = lock_bytes('HEAD')
    run('cargo', 'update', '--workspace')
    run('cargo', 'metadata', '--locked', '--format-version', '1')
    digest = validate_lock(before, Path('Cargo.lock').read_bytes())
    diff = subprocess.check_output(['git', 'diff', '--no-ext-diff', '--no-textconv', '--', 'Cargo.lock'])
    encoded = base64.b64encode(gzip.compress(diff, mtime=0)).decode('ascii')
    if len(diff) > LOCK_DIFF_LIMIT or len(encoded) > LOCK_ENCODED_LIMIT:
        raise ValueError('Cargo.lock diff exceeds workflow transport limits')
    outputs(lock_diff=encoded, lock_hash=digest)


def apply_lock(encoded, expected):
    if len(encoded) > LOCK_ENCODED_LIMIT or not re.fullmatch('[0-9a-f]{64}', expected):
        raise ValueError('Invalid lockfile transport size or digest')
    with gzip.GzipFile(fileobj=io.BytesIO(base64.b64decode(encoded, validate=True))) as compressed:
        diff = compressed.read(LOCK_DIFF_LIMIT + 1)
    if len(diff) > LOCK_DIFF_LIMIT:
        raise ValueError('Cargo.lock diff exceeds decompression limit')
    if diff:
        paths = run('git', 'apply', '--numstat', input=diff.decode('utf-8')).splitlines()
        if len(paths) != 1 or paths[0].split('\t')[2:] != ['Cargo.lock']:
            raise ValueError('Lockfile transport contains unexpected paths')
        run('git', 'apply', '--include=Cargo.lock', '-', input=diff.decode('utf-8'))
    path = Path('Cargo.lock')
    if path.is_symlink() or not path.is_file() or path.stat().st_mode & 0o111:
        raise ValueError('Cargo.lock must be a regular non-executable file')
    if validate_lock(lock_bytes('HEAD'), path.read_bytes()) != expected:
        raise ValueError('Cargo.lock differs from the verifier output')


def lock_summary(before, after):
    def packages(data):
        result = {}
        for package in tomllib.loads(data.decode('utf-8'))['package']:
            result.setdefault(package['name'], set()).add(package['version'])
        return result
    old, new = packages(before), packages(after)
    changes = [f"- {name}: {', '.join(sorted(old.get(name, []))) or '(new)'} → {', '.join(sorted(new.get(name, []))) or '(removed)'}"
               for name in sorted(old.keys() | new.keys()) if old.get(name) != new.get(name)]
    return '\n'.join(changes) or 'No package version changes.'


def validate_legacy_candidate(sha, expected_base=None, expected_generated=None):
    if not SHA.fullmatch(sha):
        raise ValueError('Expected full candidate SHA')
    parents = git('rev-list', '--parents', '-n', '1', sha).split()
    if len(parents) != 2 or (expected_base and parents[1] != expected_base):
        raise ValueError('Candidate must have the captured main as its only parent')
    files = set(git('diff-tree', '--no-commit-id', '--name-only', '-r', sha).splitlines())
    if not LEGACY_ALLOWED - {'Cargo.lock'} <= files or not files <= LEGACY_ALLOWED:
        raise ValueError(f'Unexpected candidate files: {sorted(files)}')
    identity = git('show', '-s', '--format=%an%n%ae%n%cn%n%ce', sha).splitlines()
    if identity != [BOT_NAME, BOT_EMAIL, BOT_NAME, BOT_EMAIL]:
        raise ValueError('Candidate has an unexpected author or committer')
    plan = json.loads(git('show', f'{sha}:release-plan.json'))
    if set(plan) != {'version', 'tag', 'upstream', 'generated', 'base', 'lock'}:
        raise ValueError('Unexpected release plan fields')
    if not VERSION.fullmatch(plan['version']) or plan['base'] != parents[1]:
        raise ValueError('Invalid release plan version or base')
    if validate_lock(lock_bytes(parents[1]), lock_bytes(sha)) != plan['lock']:
        raise ValueError('Release plan lock digest does not match Cargo.lock')
    tag_key(plan['tag'])
    if not all(SHA.fullmatch(plan[x]) for x in ('upstream', 'generated', 'base')):
        raise ValueError('Invalid SDK commit')
    sdk = base_at(sha)
    if sdk['tag'] != plan['tag'] or sdk['commit'] != plan['upstream']:
        raise ValueError('SDK base and release plan disagree')
    if tag_key(sdk['tag']) <= tag_key(base_at(parents[1])['tag']):
        raise ValueError('Candidate does not advance the SDK')
    if git('rev-parse', f'{sha}:tuta-repo') != plan['generated']:
        raise ValueError('SDK pin and release plan disagree')
    if expected_generated and expected_generated != plan['generated']:
        raise ValueError('SDK pin differs from the verified output')
    expected_branch = f"generated/{plan['tag'].removeprefix('tutanota-release-')}-{plan['generated'][:12]}"
    branch = git('config', '--blob', f'{sha}:.gitmodules', 'submodule.tuta-repo.branch')
    if branch != expected_branch:
        raise ValueError('SDK branch and release plan disagree')
    # The URL/path and any unrelated submodule settings must stay unchanged.
    before = git('show', f'{parents[1]}:.gitmodules')
    after = git('show', f'{sha}:.gitmodules')
    strip_branch = lambda text: re.sub(r'^\s*branch\s*=.*$', '', text, flags=re.M)
    if strip_branch(before) != strip_branch(after):
        raise ValueError('Unexpected submodule configuration changes')
    return plan



def preparation():
    spec = importlib.util.spec_from_file_location('sdk_prepare', Path(__file__).with_name('sdk-prepare.py'))
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def validate_candidate(sha, expected_base=None, expected_generated=None):
    if not SHA.fullmatch(sha):
        raise ValueError('Expected full candidate SHA')
    plan = json.loads(git('show', f'{sha}:release-plan.json'))
    if set(plan) == {'version', 'tag', 'upstream', 'generated', 'base', 'lock'}:
        return validate_legacy_candidate(sha, expected_base, expected_generated)
    if set(plan) != {'version', 'tag', 'upstream', 'tree', 'base', 'lock'}:
        raise ValueError('Unexpected release plan fields')
    parents = git('rev-list', '--parents', '-n', '1', sha).split()
    if len(parents) != 2 or (expected_base and parents[1] != expected_base):
        raise ValueError('Candidate must have the captured main as its only parent')
    files = set(git('diff-tree', '--no-commit-id', '--name-only', '-r', sha).splitlines())
    if not ALLOWED - {'Cargo.lock'} <= files or not files <= ALLOWED:
        raise ValueError(f'Unexpected candidate files: {sorted(files)}')
    if git('show', '-s', '--format=%an%n%ae%n%cn%n%ce', sha).splitlines() != [BOT_NAME, BOT_EMAIL, BOT_NAME, BOT_EMAIL]:
        raise ValueError('Candidate has an unexpected author or committer')
    if not VERSION.fullmatch(plan['version']) or plan['base'] != parents[1]:
        raise ValueError('Invalid release plan version or base')
    if not all(SHA.fullmatch(plan[x]) for x in ('upstream','tree','base')):
        raise ValueError('Invalid SDK tree or commit')
    if validate_lock(lock_bytes(parents[1]), lock_bytes(sha)) != plan['lock']:
        raise ValueError('Release plan lock digest does not match Cargo.lock')
    sdk = base_at(sha)
    if sdk['tag'] != plan['tag'] or sdk['commit'] != plan['upstream']:
        raise ValueError('SDK base and release plan disagree')
    if tag_key(sdk['tag']) <= tag_key(base_at(parents[1])['tag']):
        raise ValueError('Candidate does not advance the SDK')
    if git('rev-parse', f'{sha}:tuta-repo') != plan['upstream']:
        raise ValueError('Official SDK pin and release plan disagree')
    if expected_generated and expected_generated != plan['tree']:
        raise ValueError('SDK tree differs from the verified output')
    if git('show', f'{sha}:.gitmodules') != git('show', f'{parents[1]}:.gitmodules'):
        raise ValueError('Unexpected submodule configuration changes')
    settings = git('config','--blob',f'{sha}:.gitmodules','--list').splitlines()
    if sorted(settings) != sorted(['submodule.tuta-repo.path=tuta-repo','submodule.tuta-repo.url='+UPSTREAM]):
        raise ValueError('Expected only the official SDK submodule')
    expected = git('show',f'{sha}:sdk/EXPECTED').splitlines()
    if len(expected)<4 or expected[:2] != ['base='+plan['upstream'],'tree='+plan['tree']] or not re.fullmatch('content=[0-9a-f]{64}',expected[2]):
        raise ValueError('SDK expected tree and release plan disagree')
    patches = sorted(git('ls-tree','--name-only',sha+':sdk/patches').splitlines())
    records=[]
    for name in patches:
        if not name.endswith('.patch'): continue
        blob = subprocess.check_output(['git','show',f'{sha}:sdk/patches/{name}'])
        records.append('patch='+name+':'+hashlib.sha256(blob).hexdigest())
    if not records or expected[3:] != records:
        raise ValueError('SDK expected patches differ from candidate')
    return plan


def docker_tooling():
    spec = importlib.util.spec_from_file_location('docker_release', Path(__file__).with_name('docker-release.py'))
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def detect(args):
    base = git('rev-parse', 'HEAD')
    available = release_list()
    if not args.dry_run and 'release-plan.json' in git('ls-tree', '--name-only', base).splitlines():
        source = git('log', '-1', '--format=%H', base, '--', 'release-plan.json')
        plan = validate_candidate(source)
        existing = next((r for r in available if r['tag_name'] == 'v' + plan['version']), None)
        if existing and not existing['draft']:
            if git('rev-parse', f"refs/tags/v{plan['version']}^{{commit}}") != source:
                raise ValueError('Published plan tag does not match its source')
        if not existing or existing['draft']:
            outputs(mode='resume', source=source, version=plan['version'], base=base)
            return
    # Repair promotion of the current release before considering a new SDK.
    # Historical releases without a Docker record are deliberately ignored.
    if not args.dry_run:
        docker = docker_tooling()
        if docker.enabled():
            public = [r for r in available if not r['draft'] and VERSION.fullmatch(r['tag_name'].removeprefix('v'))]
            if public:
                newest = max(public, key=lambda r: int(VERSION.fullmatch(r['tag_name'].removeprefix('v'))[1]))
                if docker.promotion_pending(newest, docker.load_script('release-assets')):
                    version = newest['tag_name'].removeprefix('v')
                    source = git('rev-parse', f'refs/tags/v{version}^{{commit}}')
                    outputs(mode='repair', source=source, version=version, base=base)
                    return
    if args.scheduled and not scheduled_today(datetime.now(timezone.utc).date()):
        outputs(mode='skip')
        return
    current = base_at(base)['tag']
    refs = git('ls-remote', '--tags', UPSTREAM, 'refs/tags/tutanota-release-*').splitlines()
    tags = {line.split()[1].removeprefix('refs/tags/').removesuffix('^{}') for line in refs}
    tags = {t for t in tags if TAG.fullmatch(t)}
    target = args.tag or max(tags, key=tag_key)
    tag_key(target)
    matches = {ref: sha for sha, ref in (line.split() for line in refs)}
    upstream = matches.get(f'refs/tags/{target}^{{}}', matches.get(f'refs/tags/{target}'))
    if not upstream or not SHA.fullmatch(upstream):
        raise ValueError('Official release does not exist')
    if not args.dry_run and tag_key(target) < tag_key(current):
        raise ValueError('SDK downgrade refused')
    if not args.dry_run and target == current:
        outputs(mode='skip')
        return
    version = '' if args.dry_run else next_version(git('tag', '--list').splitlines(), [r['tag_name'] for r in available])
    outputs(mode='bump', base=base, tag=target, upstream=upstream, version=version)


def set_base(tag, upstream):
    tag_key(tag)
    if not SHA.fullmatch(upstream):
        raise ValueError('Invalid upstream SHA')
    p = Path('sdk/BASE')
    s = re.sub(r'^tag=.*$', f'tag={tag}', p.read_text(), flags=re.M)
    p.write_text(re.sub(r'^commit=.*$', f'commit={upstream}', s, flags=re.M))


def push(repo, sha, branch, token=None, cwd=None):
    env = os.environ.copy()
    token = token or env['GH_TOKEN']
    auth = base64.b64encode(f'x-access-token:{token}'.encode()).decode()
    env.update(GIT_CONFIG_COUNT='1', GIT_CONFIG_KEY_0='http.https://github.com/.extraheader', GIT_CONFIG_VALUE_0=f'AUTHORIZATION: basic {auth}')
    run('git', 'push', '--quiet', repo, f'{sha}:refs/heads/{branch}', env=env, cwd=cwd)


def open_pr(branch, title, body):
    prs = api(f'pulls?state=all&head=spartanz51:{branch}')
    if not prs:
        api('pulls', 'POST', dict(title=title, body=body, head=branch, base='main'))


def candidate(args):
    base = os.environ['BASE']
    if git('rev-parse', 'HEAD') != base:
        raise ValueError('Checkout is not the captured base')
    tag, upstream = os.environ['TAG'], os.environ['UPSTREAM_SHA']
    set_base(tag, upstream)
    generated = os.environ.get('GENERATED', '')
    success = os.environ.get('VERIFIED') == 'success'
    stem = tag.removeprefix('tutanota-release-')
    branch = f"sdk/{stem}-{base[:12]}" + ('' if success else '-diagnostic')
    if success:
        if not SHA.fullmatch(generated):
            raise ValueError('Invalid verifier output')
        apply_lock(os.environ['LOCK_DIFF'], os.environ['LOCK_HASH'])
        git('-C','tuta-repo','fetch','--quiet',UPSTREAM,upstream)
        git('-C','tuta-repo','checkout','--quiet','--detach',upstream)
        git('add','tuta-repo')
        prepared = preparation().prepare(Path.cwd(), update_expected=True)
        if prepared['tree'] != generated:
            raise ValueError('Regeneration differs from verified SDK')
        plan = dict(version=os.environ['VERSION'], tag=tag, upstream=upstream, tree=generated, base=base, lock=os.environ['LOCK_HASH'])
        Path('release-plan.json').write_text(json.dumps(plan, indent=2) + '\n')
        git('add', *sorted(ALLOWED))
    else:
        git('add', 'sdk/BASE')
    git('config', 'user.name', BOT_NAME)
    git('config', 'user.email', BOT_EMAIL)
    identity = dict(os.environ, GIT_AUTHOR_NAME=BOT_NAME, GIT_AUTHOR_EMAIL=BOT_EMAIL,
                    GIT_COMMITTER_NAME=BOT_NAME, GIT_COMMITTER_EMAIL=BOT_EMAIL)
    run('git', '-c', 'commit.gpgSign=false', '-c', 'core.hooksPath=/dev/null',
        'commit', '-m', f'Build on Tuta release {stem}', env=identity)
    source = git('rev-parse', 'HEAD')
    if success:
        validate_candidate(source, base, generated)
    remote = git('ls-remote', 'origin', f'refs/heads/{branch}')
    if remote:
        git('fetch', '--quiet', 'origin', f'refs/heads/{branch}')
        existing = git('rev-parse', 'FETCH_HEAD')
        if git('rev-parse', f'{existing}^') != base or git('rev-parse', f'{existing}^{{tree}}') != git('rev-parse', 'HEAD^{tree}'):
            raise ValueError('Existing branch differs; human edits are never overwritten')
        source = existing
    else:
        push(f'https://github.com/{REPO}.git', source, branch)
    if success:
        validate_candidate(source, base, generated)
    url = f"https://github.com/{REPO}/actions/runs/{os.environ['GITHUB_RUN_ID']}"
    body = f"Moves the SDK to `{tag}`.\n\n"
    body += ('Every patch passed the SDK tests. The exact candidate must pass bridge CI before automatic integration and release.' if success else '**SDK verification failed.** This diagnostic PR is never automatically integrated. Fix the patch series on main and rerun the workflow.')
    if success:
        body += '\n\nCargo.lock:\n' + lock_summary(lock_bytes(base), lock_bytes(source))
    open_pr(branch, f'Build on Tuta release {stem}', body + f'\n\n[Workflow evidence]({url}).')
    outputs(source=source if success else '', version=os.environ.get('VERSION', ''), ready=success)


def integrate(args):
    source, base, generated = os.environ['SOURCE'], os.environ['BASE'], os.environ['GENERATED']
    validate_candidate(source, base, generated)
    remote = git('ls-remote', 'origin', 'refs/heads/main').split()[0]
    if remote == source:
        return
    if remote != base:
        raise ValueError('Main advanced; run again on the new base')
    push(f'https://github.com/{REPO}.git', source, 'main')


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('command', choices=['detect', 'candidate', 'integrate', 'lock'])
    parser.add_argument('--tag', default='')
    parser.add_argument('--dry-run', action='store_true')
    parser.add_argument('--scheduled', action='store_true')
    args = parser.parse_args()
    globals()[args.command](args)
