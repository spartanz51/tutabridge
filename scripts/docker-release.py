#!/usr/bin/env python3
"""Opaque OCI transport for releases. Never execute an image in this process."""
import argparse
import base64
import contextlib
from datetime import datetime, timezone
from email.utils import parsedate_to_datetime
import time
import hashlib
import http.client
import importlib.util
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import tarfile
import tempfile
from types import SimpleNamespace
import urllib.error
import urllib.parse
import urllib.request

ROOT = Path(__file__).resolve().parents[1]
REGISTRIES = ('ghcr.io/spartanz51/tutabridge', 'docker.io/spartanz51/tutabridge')
PLATFORMS = ('amd64', 'arm64')
INDEX = 'application/vnd.oci.image.index.v1+json'
MANIFEST = 'application/vnd.oci.image.manifest.v1+json'
CONFIG = 'application/vnd.oci.image.config.v1+json'
DIGEST = re.compile(r'sha256:[0-9a-f]{64}\Z')
SHA = re.compile(r'[0-9a-f]{40}\Z')
VERSION = re.compile(r'(\d+)\.(\d+)\.(\d+)-rc\.(\d+)\Z')
RECORD = 'docker-release.json'
MAX_JSON = 4 * 1024 * 1024


def load_script(name):
    spec = importlib.util.spec_from_file_location(name, ROOT / 'scripts' / (name + '.py'))
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def enabled():
    value = json.loads((ROOT / 'docker/publication.json').read_text())
    return isinstance(value, dict) and set(value) == {'enabled'} and value['enabled'] is True


def require_enabled():
    if not enabled():
        raise ValueError('Docker publication is disabled pending maintainer approval')
    if os.environ.get('GITHUB_REF') != 'refs/heads/main':
        raise ValueError('Production automation must run from main')


def digest(data):
    return 'sha256:' + hashlib.sha256(data).hexdigest()


def encoded(value):
    return json.dumps(value, separators=(',', ':'), sort_keys=True).encode()


def identity(version, source):
    if not VERSION.fullmatch(version) or not SHA.fullmatch(source):
        raise ValueError('Invalid version or source')


def descriptor(data, media):
    return dict(mediaType=media, digest=digest(data), size=len(data))


def blob(root, desc):
    value = desc.get('digest', '')
    if desc.get('urls') or not DIGEST.fullmatch(value) or type(desc.get('size')) is not int or desc['size'] <= 0:
        raise ValueError('Invalid OCI descriptor')
    path = root / 'blobs/sha256' / value[7:]
    if path.is_symlink() or not path.is_file() or path.stat().st_size != desc['size']:
        raise ValueError('Missing OCI blob or size mismatch')
    with path.open('rb') as stream:
        if 'sha256:' + hashlib.file_digest(stream, 'sha256').hexdigest() != value:
            raise ValueError('OCI blob digest mismatch')
    return path


def json_blob(root, desc):
    path = blob(root, desc)
    if path.stat().st_size > MAX_JSON:
        raise ValueError('Oversized OCI metadata')
    return json.loads(path.read_bytes())


def extract(archive, dest):
    """Copy only OCI layout files. Never extract layers, links, or executables."""
    dest.mkdir(parents=True, exist_ok=False)
    seen = set()
    total = 0
    with tarfile.open(archive, 'r:*') as stream:
        for member in stream:
            name = member.name.removeprefix('./')
            if member.isdir() and name in ('.', 'blobs', 'blobs/sha256'):
                continue
            if (not member.isfile() or name in seen or
                    not (name in ('oci-layout', 'index.json') or re.fullmatch(r'blobs/sha256/[0-9a-f]{64}', name))):
                raise ValueError('Unsafe or duplicate OCI archive member')
            total += member.size
            if total > 4 * 1024**3 or (name in ('oci-layout', 'index.json') and member.size > MAX_JSON):
                raise ValueError('OCI archive too large')
            seen.add(name)
            path = dest / name
            path.parent.mkdir(parents=True, exist_ok=True)
            with path.open('xb') as output:
                shutil.copyfileobj(stream.extractfile(member), output)
    if json.loads((dest / 'oci-layout').read_bytes()) != {'imageLayoutVersion': '1.0.0'}:
        raise ValueError('Unsupported OCI layout')


def image(root, arch, version, source):
    index = json.loads((root / 'index.json').read_bytes())
    if index.get('schemaVersion') != 2 or len(index.get('manifests', [])) != 1:
        raise ValueError('Expected exactly one image per architecture, no embedded attestations')
    child = index['manifests'][0]
    # BuildKit may put one index between the layout index and the image.
    if child.get('mediaType') == INDEX:
        nested = json_blob(root, child)
        if nested.get('schemaVersion') != 2 or len(nested.get('manifests', [])) != 1:
            raise ValueError('Unexpected nested OCI index')
        child = nested['manifests'][0]
    if child.get('mediaType') != MANIFEST:
        raise ValueError('Expected OCI image manifest')
    manifest = json_blob(root, child)
    if manifest.get('schemaVersion') != 2 or manifest.get('config', {}).get('mediaType') != CONFIG:
        raise ValueError('Invalid OCI image config')
    config = json_blob(root, manifest['config'])
    if config.get('architecture') != arch or config.get('os') != 'linux':
        raise ValueError('Wrong image platform')
    labels = config.get('config', {}).get('Labels', {})
    for key, value in {'version': version, 'revision': source, 'source': 'https://github.com/spartanz51/tutabridge'}.items():
        if labels.get('org.opencontainers.image.' + key) != value:
            raise ValueError('OCI release labels do not match requested source/version')
    for layer in manifest.get('layers', []):
        if layer.get('urls') or layer.get('mediaType') not in (
                'application/vnd.oci.image.layer.v1.tar', 'application/vnd.oci.image.layer.v1.tar+gzip',
                'application/vnd.oci.image.layer.v1.tar+zstd'):
            raise ValueError('Unsupported or external OCI layer')
        blob(root, layer)
    return dict(mediaType=MANIFEST, digest=child['digest'], size=child['size'], platform=dict(os='linux', architecture=arch))


def sha256_file(path):
    with path.open('rb') as stream:
        return hashlib.file_digest(stream, 'sha256').hexdigest()


def single_file(path, expected=None):
    path = Path(path)
    if path.parent.is_symlink() or path.is_symlink() or not path.is_file() or list(path.parent.iterdir()) != [path]:
        raise ValueError('Artifact must contain exactly one regular file')
    if expected is not None and (not re.fullmatch(r'[0-9a-f]{64}', expected) or sha256_file(path) != expected):
        raise ValueError('Artifact differs from trusted job output')
    return path


def assemble(args):
    identity(args.version, args.source)
    target = Path(args.layout)
    target.mkdir(parents=True, exist_ok=False)
    (target / 'blobs/sha256').mkdir(parents=True)
    children, archives = [], {}
    with tempfile.TemporaryDirectory() as temporary:
        for arch in PLATFORMS:
            archive = single_file(Path(args.archives) / arch / 'image.tar', getattr(args, 'sha_' + arch, None))
            archives[arch] = sha256_file(archive)
            layout = Path(temporary) / arch
            extract(archive, layout)
            child = image(layout, arch, args.version, args.source)
            children.append(child)
            manifest = json_blob(layout, child)
            for desc in [child, manifest['config'], *manifest['layers']]:
                shutil.copyfile(blob(layout, desc), target / 'blobs/sha256' / desc['digest'][7:])
    raw = encoded(dict(schemaVersion=2, mediaType=INDEX, manifests=children))
    item = descriptor(raw, INDEX)
    (target / 'blobs/sha256' / item['digest'][7:]).write_bytes(raw)
    (target / 'index.json').write_bytes(layout_index(item))
    (target / 'oci-layout').write_bytes(encoded({'imageLayoutVersion': '1.0.0'}))
    record = dict(schema=1, version=args.version, source=args.source, sdk=args.sdk,
                  run=args.run, index=item, images=children, archives=archives)
    validate_record(record, args.version, args.source)
    Path(args.record).write_bytes(encoded(record) + b'\n')


def layout_index(item):
    return encoded(dict(schemaVersion=2, manifests=[dict(item, annotations={'org.opencontainers.image.ref.name': 'release'})]))


def validate_record(record, version=None, source=None):
    if set(record) != {'schema', 'version', 'source', 'sdk', 'run', 'index', 'images', 'archives'} or record['schema'] != 1:
        raise ValueError('Unknown Docker release record')
    identity(record['version'], record['source'])
    if (version is not None and record['version'] != version) or (source is not None and record['source'] != source):
        raise ValueError('Docker release identity mismatch')
    if not SHA.fullmatch(record['sdk']) or not re.fullmatch(r'[0-9]+/[0-9]+', record['run']):
        raise ValueError('Invalid SDK or workflow run provenance')
    if set(record['archives']) != set(PLATFORMS) or any(not re.fullmatch(r'[0-9a-f]{64}', v) for v in record['archives'].values()):
        raise ValueError('Invalid tested archive digests')
    if len(record['images']) != 2:
        raise ValueError('Both native architectures are required')
    for arch, child in zip(PLATFORMS, record['images']):
        if (set(child) != {'mediaType', 'digest', 'size', 'platform'} or child['mediaType'] != MANIFEST
                or child['platform'] != dict(os='linux', architecture=arch)
                or not DIGEST.fullmatch(child['digest']) or type(child['size']) is not int or child['size'] <= 0):
            raise ValueError('Invalid platform descriptor')
    raw = encoded(dict(schemaVersion=2, mediaType=INDEX, manifests=record['images']))
    if record['index'] != descriptor(raw, INDEX):
        raise ValueError('Canonical index does not match its children')
    return record


def request_bytes(request):
    for attempt in range(4):
        try:
            with urllib.request.urlopen(request, timeout=15) as response:
                raw = response.read(MAX_JSON + 1)
                if len(raw) > MAX_JSON:
                    raise ValueError('Oversized registry metadata')
                length = response.headers.get('Content-Length')
                if length is not None and len(raw) < int(length):
                    raise http.client.IncompleteRead(raw, int(length) - len(raw))
                return raw
        except (OSError, http.client.HTTPException) as error:
            transient = not isinstance(error, urllib.error.HTTPError) or error.code == 429 or 500 <= error.code < 600
            if not transient or attempt == 3:
                raise
            delay = 2 ** attempt
            retry = error.headers.get('Retry-After') if isinstance(error, urllib.error.HTTPError) else None
            if retry:
                try:
                    delay = max(delay, int(retry))
                except ValueError:
                    try:
                        delay = max(delay, (parsedate_to_datetime(retry) - datetime.now(timezone.utc)).total_seconds())
                    except (ValueError, TypeError, OverflowError):
                        pass
            if delay > 30:
                raise ValueError('Registry asks for a retry beyond the 30s retry budget; retry this workflow later') from None
            time.sleep(delay)


class Registry:
    """Bounded registry reads. Only a real 404 means absent; auth errors propagate."""
    def __init__(self, reference, credentials=None):
        host, self.repo = reference.split('/', 1)
        endpoints = {'ghcr.io': ('https://ghcr.io', 'https://ghcr.io/token'),
                     'docker.io': ('https://registry-1.docker.io', 'https://auth.docker.io/token')}
        self.base, self.realm = endpoints[host]
        self.credentials = credentials
        self.token = None

    def get(self, route):
        url = self.base + '/v2/' + self.repo + '/' + route
        headers = {'Accept': INDEX + ', ' + MANIFEST}
        for attempt in range(2):
            if self.token:
                headers['Authorization'] = 'Bearer ' + self.token
            try:
                return request_bytes(urllib.request.Request(url, headers=headers))
            except urllib.error.HTTPError as error:
                if error.code == 404:
                    return None
                if error.code != 401 or attempt:
                    raise ValueError(f'Registry request failed with HTTP {error.code}') from None
                challenge = error.headers.get('WWW-Authenticate', '')
                fields = dict(re.findall(r'(\w+)="([^"]*)"', challenge))
                if not challenge.startswith('Bearer ') or fields.get('realm') != self.realm:
                    raise ValueError('Unexpected registry authentication endpoint') from None
                query = urllib.parse.urlencode({'service': fields.get('service', ''), 'scope': 'repository:' + self.repo + ':pull'})
                auth_headers = {}
                if self.credentials:
                    auth_headers['Authorization'] = 'Basic ' + base64.b64encode(':'.join(self.credentials).encode()).decode()
                try:
                    auth = json.loads(request_bytes(urllib.request.Request(self.realm + '?' + query, headers=auth_headers)))
                    self.token = auth.get('token', auth.get('access_token'))
                    if not isinstance(self.token, str) or not self.token:
                        raise ValueError('Missing registry pull token')
                except urllib.error.HTTPError as auth_error:
                    raise ValueError(f'Registry authentication failed with HTTP {auth_error.code}') from None
        raise ValueError('Registry authentication failed')

    def manifest(self, reference):
        raw = self.get('manifests/' + reference)
        return None if raw is None else (digest(raw), json.loads(raw), raw)


def inspect_record(registry, record, reference):
    result = registry.manifest(reference)
    if result is None:
        return False
    expected = record['index']
    if result[0] != expected['digest'] or len(result[2]) != expected['size']:
        raise ValueError('Registry reference has a different canonical digest')
    for child in record['images']:
        actual = registry.manifest(child['digest'])
        if actual is None or actual[0] != child['digest'] or len(actual[2]) != child['size']:
            raise ValueError('Registry child is missing or corrupted')
    return True


def registries(auth=False):
    credentials = (None, None)
    if auth:
        github = os.environ.get('GH_TOKEN', '')
        hub = os.environ.get('DOCKERHUB_TOKEN', '')
        if not github or not hub:
            raise ValueError('Missing named registry credential; no write attempted')
        credentials = ((os.environ['GITHUB_ACTOR'], github), ('spartanz51', hub))
    return [Registry(ref, cred) for ref, cred in zip(REGISTRIES, credentials)]


def read_record(args):
    path = Path(args.record)
    expected = getattr(args, 'record_sha', None)
    if expected is not None:
        single_file(path, expected)
    return validate_record(json.loads(path.read_bytes()), args.version, args.source)


def check_record(args):
    single_file(Path(args.record), args.record_sha)
    read_record(args)


def verify(args):
    record = read_record(args)
    for registry in registries():
        if not inspect_record(registry, record, args.version):
            raise ValueError('Version is not anonymously available in both registries')
    if args.output:
        Path(args.output).write_bytes(encoded(record) + b'\n')


def durable_bytes(release, assets):
    matches = [asset for asset in release.get('assets', []) if asset['name'] == RECORD]
    if not matches:
        return None
    if len(matches) != 1 or matches[0]['size'] > MAX_JSON:
        raise ValueError('Invalid durable Docker record')
    raw = subprocess.check_output(['gh', 'api', f"repos/{assets.REPO}/releases/assets/{matches[0]['id']}", '-H', 'Accept: application/octet-stream'])
    validate_record(json.loads(raw))
    return raw


def durable_record(release, assets):
    raw = durable_bytes(release, assets)
    return validate_record(json.loads(raw)) if raw is not None else None


def preflight(args):
    identity(args.version, args.source)
    # This command runs only in the trusted contents:write planning job. GitHub
    # deliberately hides drafts from contents:read installation tokens.
    assets = load_script('release-assets')
    assets.check_source(args)
    release, _tagged = assets.release_state(args.version, args.source)
    # release_state already pins a pre-existing tag to source. A failed release
    # POST after tag creation must remain recoverable, including desktop-only.
    docker = enabled()
    if not docker and release and release['draft'] and any(
            asset['name'] == RECORD for asset in release.get('assets', [])):
        raise ValueError('Draft reserved for Docker: finish it with Docker enabled or have a maintainer explicitly abandon this version')
    mode = 'desktop'
    if docker:
        require_enabled()
        raw = durable_bytes(release, assets) if release else None
        if raw is not None:
            validate_record(json.loads(raw), args.version, args.source)
            Path(args.record).parent.mkdir(parents=True, exist_ok=True)
            Path(args.record).write_bytes(raw)
        if release and not release['draft'] and raw is None:
            raise ValueError('Published release predates Docker; select a new version')
        mode = 'repair' if release and not release['draft'] else 'resume' if raw is not None else 'build'
        if raw is not None:
            with open(os.environ['GITHUB_OUTPUT'], 'a') as stream:
                stream.write('record_sha=' + digest(raw)[7:] + '\n')
    with open(os.environ['GITHUB_OUTPUT'], 'a') as stream:
        stream.write('mode=' + mode + '\n')


def attach_record(args):
    """Reserve the canonical digest in a draft before any versioned registry push."""
    require_enabled()
    record = read_record(args)
    assets = load_script('release-assets')
    assets.check_source(args)
    release, tagged = assets.release_state(args.version, args.source)
    if release:
        existing = durable_bytes(release, assets)
        if existing is not None:
            if existing != Path(args.record).read_bytes():
                raise ValueError('A canonical record already exists; recover its original OCI bytes')
            return
        if not release['draft']:
            raise ValueError('Never add artifacts to a published release')
    run_id, attempt = record['run'].split('/')
    if run_id != os.environ.get('GITHUB_RUN_ID', '') or int(attempt) > int(os.environ.get('GITHUB_RUN_ATTEMPT', '0')):
        raise ValueError('New canonical record must originate in this workflow run')
    # Validate both remote destinations BEFORE changing the draft or either tag.
    for registry in registries(auth=True):
        if registry.manifest(args.version) is not None:
            raise ValueError('Versioned image exists without a durable record; investigate')
    # Reconstruct from the exact tested archives BEFORE reserving a new record.
    with recovered_layout(args, record):
        pass
    assets.ensure_draft(args.version, args.source)
    assets.run('gh', 'release', 'upload', 'v' + args.version, args.record, '--repo', assets.REPO)
    saved, _ = assets.release_state(args.version, args.source)
    if durable_bytes(saved, assets) != Path(args.record).read_bytes():
        raise ValueError('Durable Docker record read-back failed')


def registry_preflight(args):
    """Fresh trusted runner: read both destinations before building or signing."""
    require_enabled()
    identity(args.version, args.source)
    record = read_record(args) if Path(args.record).exists() else None
    for reader in registries(auth=True):
        if record:
            inspect_record(reader, record, args.version)
        elif reader.manifest(args.version) is not None:
            raise ValueError('Versioned image exists without a durable record; investigate')


def validate_layout(layout, record):
    # Validate every referenced blob again in the privileged process. An
    # artifact is data, even when a previous read-only job already checked it.
    if (layout / 'index.json').read_bytes() != layout_index(record['index']):
        raise ValueError('Layout index differs from reserved index')
    raw = blob(layout, record['index']).read_bytes()
    if raw != encoded(dict(schemaVersion=2, mediaType=INDEX, manifests=record['images'])):
        raise ValueError('Recovered layout is not the reserved index')
    for child in record['images']:
        # image() expects a single-platform layout index. Build a temporary
        # metadata view sharing only validated blobs, never extracting layers.
        with tempfile.TemporaryDirectory() as temporary:
            view = Path(temporary)
            (view / 'blobs').symlink_to((layout / 'blobs').resolve(), target_is_directory=True)
            (view / 'index.json').write_bytes(encoded(dict(schemaVersion=2, manifests=[child])))
            if image(view, child['platform']['architecture'], record['version'], record['source']) != child:
                raise ValueError('Recovered image differs from the reserved descriptor')


@contextlib.contextmanager
def recovered_layout(args, record):
    # Never reuse a caller-owned layout or a mutable :release selector.
    with tempfile.TemporaryDirectory() as temporary:
        root = Path(temporary)
        archives = Path(args.archives)
        if not all((archives / arch / 'image.tar').exists() for arch in PLATFORMS):
            archives = root / 'archives'
            run_id, _attempt = record['run'].split('/')
            for arch in PLATFORMS:
                subprocess.run(['gh', 'run', 'download', run_id, '--repo', 'spartanz51/tutabridge',
                                '--name', f'docker-{arch}', '--dir', str(archives / arch)], check=True)
        layout = root / 'layout'
        recovered = root / RECORD
        assemble(SimpleNamespace(version=args.version, source=args.source, sdk=record['sdk'],
            run=record['run'], archives=archives, layout=layout, record=recovered,
            sha_amd64=record['archives']['amd64'], sha_arm64=record['archives']['arm64']))
        if recovered.read_bytes() != Path(args.record).read_bytes():
            raise ValueError('Recovered original OCI artifacts differ from the reserved record')
        validate_layout(layout, record)
        yield layout


def transport(args):
    require_enabled()
    record = read_record(args)
    readers = registries(auth=True)
    present = [inspect_record(reader, record, args.version) for reader in readers]
    with contextlib.ExitStack() as stack:
        if any(present):
            source = REGISTRIES[present.index(True)] + '@' + record['index']['digest']
        else:
            layout = stack.enter_context(recovered_layout(args, record))
            source = 'ocidir://' + str(layout.resolve()) + '@' + record['index']['digest']
        for reference, reader, exists in zip(REGISTRIES, readers, present):
            if not exists:
                subprocess.run(['regctl', 'image', 'copy', source, reference + ':' + args.version], check=True)
            if not inspect_record(reader, record, args.version):
                raise ValueError('Versioned image push read-back failed')


def promotion_pending(release, assets):
    record = durable_record(release, assets)
    if record is None:
        return False  # Historical releases are not retroactively dockerized.
    validate_record(record, release['tag_name'].removeprefix('v'))
    readers = registries()
    for reader in readers:
        if not inspect_record(reader, record, record['version']):
            return True  # Schedule repair before a new SDK; restore the reserved bytes.
    return any((result := reader.manifest('latest')) is None or result[0] != record['index']['digest'] for reader in readers)


def latest(args):
    require_enabled()
    record = read_record(args)
    assets = load_script('release-assets')
    release, _ = assets.release_state(args.version, args.source)
    current = assets.api('releases/latest')
    if not release or release['draft'] or current['tag_name'] != 'v' + args.version:
        raise ValueError('Refusing to promote any release other than the current public release')
    if durable_bytes(release, assets) != Path(args.record).read_bytes():
        raise ValueError('Published Docker record mismatch')
    readers = registries(auth=True)
    for reader in readers:
        if not inspect_record(reader, record, args.version):
            raise ValueError('Version missing before promotion')
    for reference in REGISTRIES:
        # A digest, never a potentially moved tag, is the copy source.
        subprocess.run(['regctl', 'image', 'copy', reference + '@' + record['index']['digest'], reference + ':latest'], check=True)
    for reader in readers:
        if not inspect_record(reader, record, 'latest'):
            raise ValueError('latest promotion incomplete')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('command', choices=['enabled', 'check-record', 'assemble', 'preflight', 'registry-preflight', 'attach-record', 'transport', 'verify', 'latest'])
    parser.add_argument('--version')
    parser.add_argument('--source')
    parser.add_argument('--record', default=RECORD)
    parser.add_argument('--archives', default='docker-archives')
    parser.add_argument('--layout', default='docker-layout')
    parser.add_argument('--sdk')
    parser.add_argument('--run')
    parser.add_argument('--output')
    parser.add_argument('--record-sha')
    parser.add_argument('--sha-amd64')
    parser.add_argument('--sha-arm64')
    args = parser.parse_args()
    if args.command == 'enabled':
        print(str(enabled()).lower())
        return
    if not args.version or not args.source:
        parser.error('--version and --source are required for this command')
    globals()[args.command.replace('-', '_')](args)


if __name__ == '__main__':
    main()
