"""Offline failure/recovery tests for the actual OCI publisher. No real credentials."""
import contextlib
import importlib.util
import io
import http.client
import ssl
import json
import os
from pathlib import Path
import subprocess
import tarfile
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch, Mock
import urllib.error

SCRIPTS = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location('docker_release', SCRIPTS / 'docker-release.py')
d = importlib.util.module_from_spec(spec)
spec.loader.exec_module(d)
VERSION = '0.1.0-rc.99'
SOURCE = 'a' * 40


def archive(path, arch, source=SOURCE, version=VERSION):
    values = {}
    def add(value, media):
        raw = d.encoded(value)
        desc = d.descriptor(raw, media)
        values['blobs/sha256/' + desc['digest'][7:]] = raw
        return desc
    config = add(dict(architecture=arch, os='linux', rootfs=dict(type='layers', diff_ids=[]),
                      config=dict(Labels={'org.opencontainers.image.revision': source,
                          'org.opencontainers.image.version': version,
                          'org.opencontainers.image.source': 'https://github.com/spartanz51/tutabridge'})), d.CONFIG)
    manifest = add(dict(schemaVersion=2, mediaType=d.MANIFEST, config=config, layers=[]), d.MANIFEST)
    values['index.json'] = d.encoded(dict(schemaVersion=2, manifests=[manifest]))
    values['oci-layout'] = d.encoded({'imageLayoutVersion': '1.0.0'})
    path.parent.mkdir(parents=True, exist_ok=True)
    with tarfile.open(path, 'w') as stream:
        for name, raw in values.items():
            member = tarfile.TarInfo(name)
            member.size = len(raw)
            stream.addfile(member, io.BytesIO(raw))


class FakeRegistry:
    def __init__(self, record, version=True, latest=False):
        self.record = record
        self.version = version
        self.latest = latest
        self.wrong = False
    def manifest(self, reference):
        if reference in (VERSION, 'latest'):
            if not (self.version if reference == VERSION else self.latest):
                return None
            raw = d.encoded(dict(schemaVersion=2, mediaType=d.INDEX, manifests=self.record['images']))
            return ('sha256:' + 'f'*64 if self.wrong else d.digest(raw), json.loads(raw), raw)
        for child in self.record['images']:
            if reference == child['digest']:
                # The metadata body is immaterial to inspect_record, but sizes and
                # content digests come from the real fixture layout below.
                raw = self.layout.joinpath('blobs/sha256', reference[7:]).read_bytes()
                return d.digest(raw), json.loads(raw), raw
        return None


class Publisher(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        for arch in d.PLATFORMS:
            archive(self.root / arch / 'image.tar', arch)
        self.args = SimpleNamespace(version=VERSION, source=SOURCE, sdk='b'*40, run='12/1',
            archives=str(self.root), layout=str(self.root/'layout'), record=str(self.root/d.RECORD), output=None, publish=False)
        d.assemble(self.args)
        self.record = json.loads(Path(self.args.record).read_bytes())
        self.readers = [FakeRegistry(self.record), FakeRegistry(self.record)]
        for reader in self.readers:
            reader.layout = Path(self.args.layout)

    def test_exact_platforms_and_index_are_bound(self):
        self.assertEqual(d.validate_record(self.record, VERSION, SOURCE), self.record)
        self.record['images'][0]['platform']['architecture'] = 'arm64'
        with self.assertRaises(ValueError): d.validate_record(self.record)

    def test_record_rejects_extra_destination_or_new_digest(self):
        for field, value in [('registry', 'evil.invalid/repo'), ('source', 'c'*40)]:
            item = dict(self.record, **{field: value})
            with self.assertRaises(ValueError): d.validate_record(item, VERSION, SOURCE)
        self.record['index']['digest'] = 'sha256:'+'0'*64
        with self.assertRaises(ValueError): d.validate_record(self.record)

    def test_wrong_label_and_wrong_architecture_are_rejected(self):
        for arch, source in [('arm64', SOURCE), ('amd64', 'c'*40)]:
            target = self.root / ('test-' + arch + source[0])
            archive(self.root/'bad.tar', arch, source)
            d.extract(self.root/'bad.tar', target)
            with self.assertRaises(ValueError): d.image(target, 'amd64', VERSION, SOURCE)

    def test_paths_symlinks_duplicates_and_corrupt_blobs_rejected(self):
        for index, name in enumerate(('../escape', '/escape', 'index.json')):
            path = self.root / 'unsafe.tar'
            with tarfile.open(path, 'w') as stream:
                member = tarfile.TarInfo(name)
                member.type = tarfile.SYMTYPE
                member.linkname = '/tmp/escape'
                stream.addfile(member)
            with self.assertRaises(ValueError): d.extract(path, self.root / f'unsafe-{index}')
        child = self.record['images'][0]
        (Path(self.args.layout)/'blobs/sha256'/child['digest'][7:]).write_bytes(b'tampered')
        with self.assertRaises(ValueError): d.blob(Path(self.args.layout), child)

    def test_disabled_switch_prevents_any_network_or_write(self):
        with patch.object(d, 'enabled', return_value=False), patch.object(d, 'registries') as readers, patch.object(d.subprocess, 'run') as call:
            for operation in (d.attach_record, d.transport, d.latest):
                with self.assertRaisesRegex(ValueError, 'disabled'): operation(self.args)
            readers.assert_not_called()
            call.assert_not_called()

    def test_different_digest_same_source_stops_before_either_write(self):
        self.readers[1].wrong = True
        with patch.object(d, 'require_enabled'), patch.object(d, 'registries', return_value=self.readers), patch.object(d.subprocess, 'run') as copy:
            with self.assertRaisesRegex(ValueError, 'different canonical digest'): d.transport(self.args)
            copy.assert_not_called()

    def test_partial_push_reuses_original_digest_not_local_rebuild(self):
        self.readers[1].version = False
        calls = []
        def copy(args, **kwargs):
            calls.append(args)
            self.readers[1].version = True
        with patch.object(d, 'require_enabled'), patch.object(d, 'registries', return_value=self.readers), patch.object(d.subprocess, 'run', side_effect=copy):
            self.args.layout = str(self.root/'does-not-exist')
            d.transport(self.args)
            d.transport(self.args)
        self.assertEqual(len(calls), 1)
        self.assertEqual(calls[0][3], d.REGISTRIES[0]+'@'+self.record['index']['digest'])
        self.assertEqual(calls[0][4], d.REGISTRIES[1]+':'+VERSION)

    def test_new_push_failure_leaves_original_available_for_retry(self):
        self.readers[0].version = self.readers[1].version = False
        def copy(args, **kwargs):
            if args[-1].startswith(d.REGISTRIES[1]): raise RuntimeError('mirror offline')
            self.readers[0].version = True
        with patch.object(d, 'require_enabled'), patch.object(d, 'registries', return_value=self.readers), patch.object(d.subprocess, 'run', side_effect=copy):
            with self.assertRaises(RuntimeError): d.transport(self.args)
        self.assertTrue(self.readers[0].version)
        self.assertFalse(self.readers[1].version)

    def test_missing_original_artifacts_never_trigger_a_rebuild(self):
        self.readers[0].version = self.readers[1].version = False
        self.args.archives = str(self.root/'missing')
        with patch.object(d, 'require_enabled'), patch.object(d, 'registries', return_value=self.readers), patch.object(d.subprocess, 'run', side_effect=RuntimeError('artifact expired')) as call:
            with self.assertRaises(RuntimeError): d.transport(self.args)
        self.assertEqual(call.call_args.args[0][:4], ['gh', 'run', 'download', '12'])

    def test_verify_uses_anonymous_readers_and_checks_both(self):
        with patch.object(d, 'registries', return_value=self.readers) as readers:
            d.verify(self.args)
            readers.assert_called_once_with()
            self.readers[1].version = False
            with self.assertRaises(ValueError): d.verify(self.args)

    def test_latest_old_rerun_cannot_regress(self):
        assets = Mock()
        assets.release_state.return_value = ({'draft':False}, True)
        assets.api.return_value = {'tag_name': 'v0.1.0-rc.100'}
        with patch.object(d, 'require_enabled'), patch.object(d, 'load_script', return_value=assets), patch.object(d.subprocess, 'run') as copy:
            with self.assertRaisesRegex(ValueError, 'current public'): d.latest(self.args)
            copy.assert_not_called()

    def test_latest_partial_failure_is_detected_and_retry_repairs(self):
        assets = Mock()
        assets.release_state.return_value = ({'draft':False}, True)
        assets.api.return_value = {'tag_name': 'v'+VERSION}
        def copy(args, **kwargs):
            self.readers[0 if args[-1].startswith(d.REGISTRIES[0]) else 1].latest = True
        with patch.object(d, 'require_enabled'), patch.object(d, 'load_script', return_value=assets), patch.object(d, 'durable_record', return_value=self.record), patch.object(d, 'durable_bytes', return_value=Path(self.args.record).read_bytes()), patch.object(d, 'registries', return_value=self.readers), patch.object(d.subprocess, 'run', side_effect=copy):
            self.assertTrue(d.promotion_pending({'tag_name':'v'+VERSION}, assets))
            d.latest(self.args)
            self.assertFalse(d.promotion_pending({'tag_name':'v'+VERSION}, assets))

    def test_existing_durable_record_cannot_be_replaced(self):
        assets = Mock()
        assets.release_state.return_value = ({'draft':True}, True)
        with patch.object(d, 'require_enabled'), patch.object(d, 'load_script', return_value=assets), patch.object(d, 'durable_bytes', return_value=d.encoded(dict(self.record, source='c'*40))):
            with self.assertRaisesRegex(ValueError, 'canonical record'): d.attach_record(self.args)
            assets.ensure_draft.assert_not_called()

    def test_registry_preflight_checks_both_without_mutation(self):
        with patch.object(d, 'require_enabled'), patch.object(d, 'registries', return_value=self.readers), patch.object(d.subprocess, 'run') as run:
            d.registry_preflight(self.args)
            self.readers[1].wrong = True
            with self.assertRaises(ValueError): d.registry_preflight(self.args)
            run.assert_not_called()

    def test_recovery_revalidates_config_blob_before_any_copy(self):
        self.readers[0].version = self.readers[1].version = False
        path = self.root/'amd64/image.tar'
        with tarfile.open(path) as stream:
            entries = {item.name: stream.extractfile(item).read() for item in stream if item.isfile()}
        child = json.loads(entries['index.json'])['manifests'][0]
        config = json.loads(entries['blobs/sha256/'+child['digest'][7:]])['config']
        name = 'blobs/sha256/'+config['digest'][7:]
        entries[name] = entries[name].replace(b'linux', b'linuz')  # same size, wrong config hash
        with tarfile.open(path, 'w') as stream:
            for name, raw in entries.items():
                item=tarfile.TarInfo(name); item.size=len(raw); stream.addfile(item,io.BytesIO(raw))
        # Bind the altered archive, so this test reaches the inner blob check.
        self.record['archives']['amd64'] = d.sha256_file(path)
        Path(self.args.record).write_bytes(d.encoded(self.record)+b'\n')
        with patch.object(d, 'require_enabled'), patch.object(d, 'registries', return_value=self.readers), patch.object(d.subprocess, 'run') as run:
            with self.assertRaisesRegex(ValueError, 'OCI blob digest mismatch'): d.transport(self.args)
            run.assert_not_called()

    def test_preflight_selects_build_resume_and_repair_from_durable_record(self):
        assets=Mock()
        self.args.publish=True
        output=self.root/'output'
        with patch.dict(os.environ, GITHUB_OUTPUT=str(output)), patch.object(d, 'require_enabled'), patch.object(d, 'enabled', return_value=True), patch.object(d, 'load_script', return_value=assets):
            for release,record,mode in [(None,None,'build'),({'draft':True},self.record,'resume'),({'draft':False},self.record,'repair')]:
                assets.release_state.return_value=(release,release is not None)
                with patch.object(d,'durable_bytes',return_value=d.encoded(record)+b'\n' if record else None): d.preflight(self.args)
                self.assertTrue(output.read_text().endswith('mode='+mode+'\n'))
            assets.release_state.return_value=({'draft':False},True)
            with patch.object(d,'durable_bytes',return_value=None):
                with self.assertRaisesRegex(ValueError,'predates Docker'): d.preflight(self.args)

    def test_only_404_means_missing(self):
        reader = d.Registry(d.REGISTRIES[0])
        for status in (401, 403, 429, 500, 404):
            error = urllib.error.HTTPError('https://ghcr.io', status, 'fixture', {}, None)
            with patch.object(d.urllib.request, 'urlopen', side_effect=error), patch.object(d.time, 'sleep'):
                if status == 404:
                    self.assertIsNone(reader.manifest(VERSION))
                else:
                    with self.assertRaises(ValueError): reader.manifest(VERSION)

    def test_registry_rejects_untrusted_token_endpoint(self):
        error = urllib.error.HTTPError('https://ghcr.io', 401, 'fixture', {'WWW-Authenticate': 'Bearer realm="https://evil.invalid/token"'}, None)
        with patch.object(d.urllib.request, 'urlopen', side_effect=error) as request:
            with self.assertRaisesRegex(ValueError, 'authentication endpoint'):
                d.Registry(d.REGISTRIES[0], ('fixture','not-a-real-token')).manifest(VERSION)
        self.assertEqual(request.call_count, 1)


    def test_assembly_discards_unreferenced_blobs(self):
        raw=b'unreferenced blob'
        path='blobs/sha256/'+d.digest(raw)[7:]
        with tarfile.open(self.root/'amd64/image.tar','a') as stream:
            item=tarfile.TarInfo(path);item.size=len(raw);stream.addfile(item,io.BytesIO(raw))
        self.args.layout=self.root/'clean-layout'
        d.assemble(self.args)
        self.assertFalse((self.args.layout/path).exists())

    def test_record_artifact_cannot_install_scripts_or_shadow_imports(self):
        root = self.root/'download'; root.mkdir()
        record = root/d.RECORD; record.write_bytes(Path(self.args.record).read_bytes())
        expected = d.sha256_file(record)
        d.single_file(record, expected)
        for name in ('scripts/docker-release.py', 'scripts/json.py'):
            evil = root/name; evil.parent.mkdir(exist_ok=True); evil.write_text('raise SystemExit("attacker")')
            with self.assertRaisesRegex(ValueError, 'exactly one'): d.single_file(record, expected)
            evil.unlink()
        (root/'scripts').rmdir()
        record.write_bytes(record.read_bytes()+b' ')
        with self.assertRaisesRegex(ValueError, 'trusted job output'): d.single_file(record, expected)

    def test_tested_archive_digest_is_required_before_assembly(self):
        self.args.sha_amd64 = '0'*64
        self.args.layout = self.root/'new-layout'
        with self.assertRaisesRegex(ValueError, 'trusted job output'): d.assemble(self.args)

    def test_layout_selector_is_validated_and_transport_uses_digest(self):
        layout = Path(self.args.layout)
        (layout/'index.json').write_bytes(b'{}')
        with self.assertRaisesRegex(ValueError, 'Layout index'): d.validate_layout(layout, self.record)
        self.readers[0].version = self.readers[1].version = False
        sources=[]
        def copy(argv, **kwargs):
            sources.append(argv[3])
            self.readers[0 if argv[-1].startswith(d.REGISTRIES[0]) else 1].version=True
        with patch.object(d,'require_enabled'), patch.object(d,'registries',return_value=self.readers), patch.object(d.subprocess,'run',side_effect=copy):
            d.transport(self.args) # discards that layout and rebuilds from archived bytes
        self.assertTrue(all(src.endswith('@'+self.record['index']['digest']) for src in sources))

    def test_new_record_is_not_reserved_before_revalidating_archives(self):
        assets=Mock(); assets.release_state.return_value=(None,False)
        for reader in self.readers: reader.version=False
        (self.root/'amd64/image.tar').write_bytes(b'changed')
        with patch.dict(os.environ,GITHUB_RUN_ID='12',GITHUB_RUN_ATTEMPT='1'), patch.object(d,'require_enabled'), patch.object(d,'load_script',return_value=assets), patch.object(d,'registries',return_value=self.readers):
            with self.assertRaisesRegex(ValueError,'trusted job output'): d.attach_record(self.args)
        assets.ensure_draft.assert_not_called(); assets.run.assert_not_called()

    def test_durable_comparison_preserves_exact_bytes(self):
        assets=Mock(); assets.release_state.return_value=({'draft':True},True)
        alternate=json.dumps(self.record, indent=2).encode()
        with patch.object(d,'require_enabled'), patch.object(d,'load_script',return_value=assets), patch.object(d,'durable_bytes',return_value=alternate):
            with self.assertRaisesRegex(ValueError,'canonical record'): d.attach_record(self.args)
        assets.run.assert_not_called()

    def test_desktop_planning_accepts_unreserved_versions_and_rejects_reserved_drafts(self):
        assets=Mock()
        output=self.root/'output'
        for release, tagged, reserved in (
            (None, False, False), (None, True, False),
            ({'draft':True,'assets':[]}, True, False),
            ({'draft':True,'assets':[{'name':d.RECORD}]}, True, True),
            ({'draft':False,'assets':[{'name':d.RECORD}]}, True, False),
        ):
            assets.release_state.return_value=(release,tagged)
            output.unlink(missing_ok=True)
            with self.subTest(release=release,tagged=tagged), patch.dict(os.environ,GITHUB_OUTPUT=str(output)), patch.object(d,'enabled',return_value=False), patch.object(d,'load_script',return_value=assets), patch.object(d,'registries') as registries, patch.object(d,'durable_bytes') as read:
                if reserved:
                    with self.assertRaisesRegex(ValueError,'Draft reserved for Docker'): d.preflight(self.args)
                    self.assertFalse(output.exists())
                else:
                    d.preflight(self.args)
                    self.assertEqual(output.read_text(),'mode=desktop\n')
                registries.assert_not_called(); read.assert_not_called()
                assets.check_source.assert_called_with(self.args)
                assets.release_state.assert_called_with(VERSION,SOURCE)

    def test_docker_planning_recovers_a_tag_without_a_release(self):
        assets=Mock(); assets.release_state.return_value=(None,True)
        output=self.root/'output'
        with patch.dict(os.environ,GITHUB_OUTPUT=str(output)), patch.object(d,'enabled',return_value=True), patch.object(d,'require_enabled'), patch.object(d,'load_script',return_value=assets):
            d.preflight(self.args)
        self.assertEqual(output.read_text(),'mode=build\n')

    def test_switch_is_strict_and_independent_of_repository_configuration(self):
        (self.root/'docker').mkdir()
        with patch.object(d,'ROOT',self.root):
            for value in ({'enabled':True},{'enabled':False},{'enabled':1},{'enabled':'true'}, {'enabled':True,'extra':1},[],None):
                (self.root/'docker/publication.json').write_text(json.dumps(value))
                self.assertEqual(d.enabled(), value == {'enabled':True} and type(value.get('enabled')) is bool if isinstance(value,dict) else False)

    def test_missing_published_mirror_routes_to_repair(self):
        self.readers[1].version=False
        with patch.object(d,'durable_record',return_value=self.record), patch.object(d,'registries',return_value=self.readers):
            self.assertTrue(d.promotion_pending({'tag_name':'v'+VERSION},Mock()))

    def test_transient_reads_retry_with_retry_after_but_auth_does_not(self):
        error=urllib.error.HTTPError('fixture',429,'fixture',{'Retry-After':'3'},None)
        response=Mock(); response.__enter__=Mock(return_value=response); response.__exit__=Mock(); response.read.return_value=b'{}'; response.headers={}
        with patch.object(d.urllib.request,'urlopen',side_effect=[error,TimeoutError(),response]) as request, patch.object(d.time,'sleep') as sleep:
            self.assertEqual(d.request_bytes('fixture'),b'{}')
        self.assertEqual(request.call_count,3); self.assertEqual([c.args[0] for c in sleep.call_args_list],[3,2])
        for code in (401,403):
            with patch.object(d.urllib.request,'urlopen',side_effect=urllib.error.HTTPError('fixture',code,'fixture',{},None)) as request, patch.object(d.time,'sleep') as sleep:
                with self.assertRaises(urllib.error.HTTPError): d.request_bytes('fixture')
                self.assertEqual(request.call_count,1); sleep.assert_not_called()

    def test_transport_errors_during_headers_or_body_are_retried_and_bounded(self):
        errors = [ConnectionResetError(), http.client.RemoteDisconnected(), ssl.SSLError(), http.client.IncompleteRead(b'{', 4)]
        for error in errors:
            for stage in ('open', 'read'):
                with self.subTest(error=type(error).__name__,stage=stage):
                    response=Mock(); response.__enter__=Mock(return_value=response); response.__exit__=Mock(); response.headers={}
                    if stage == 'read': response.read.side_effect=[error,b'{}']
                    else: response.read.return_value=b'{}'
                    with patch.object(d.urllib.request,'urlopen',side_effect=[error,response] if stage == 'open' else [response,response]) as request, patch.object(d.time,'sleep'):
                        self.assertEqual(d.request_bytes('fixture'),b'{}'); self.assertEqual(request.call_count,2)
            with patch.object(d.urllib.request,'urlopen',side_effect=error) as request, patch.object(d.time,'sleep'):
                with self.assertRaises(type(error)): d.request_bytes('fixture')
                self.assertEqual(request.call_count,4)

    def test_truncated_content_length_is_retried_before_json_decode(self):
        response=Mock(); response.__enter__=Mock(return_value=response); response.__exit__=Mock(); response.headers={'Content-Length':'2'}
        response.read.side_effect=[b'{',b'{}']
        with patch.object(d.urllib.request,'urlopen',return_value=response) as request, patch.object(d.time,'sleep'):
            self.assertEqual(d.request_bytes('fixture'),b'{}'); self.assertEqual(request.call_count,2)


@unittest.skipUnless(os.environ.get('TUTABRIDGE_TEST_REGISTRY') == '1', 'opt-in disposable local registry test')
class LocalTransport(Publisher):
    # Inherit the offline checks and add a real regctl roundtrip with no secrets.
    def test_real_copy_between_disposable_local_registries(self):
        names = []
        env = dict(os.environ, REGCTL_CONFIG=str(self.root/'regctl.json'))
        def command(*args):
            return subprocess.check_output(args, env=env, text=True).strip()
        try:
            references = []
            readers = []
            for i in range(2):
                name = 'tutabridge-oci-test-' + self.root.name + str(i)
                names.append(name)
                command('docker', 'run', '-d', '--name', name, '-p', '127.0.0.1::5000', 'registry:2')
                host = command('docker', 'port', name, '5000/tcp').strip()
                command('regctl', 'registry', 'set', host, '--tls', 'disabled')
                references.append(host+'/test')
                reader = object.__new__(d.Registry)
                reader.base, reader.repo, reader.realm, reader.token, reader.credentials = 'http://'+host, 'test', '', None, None
                readers.append(reader)
            # Production destinations are not configurable by artifacts. Only
            # this test substitutes loopback endpoints at the module boundary.
            with patch.object(d, 'require_enabled'), patch.object(d, 'REGISTRIES', tuple(references)), patch.object(d, 'registries', return_value=readers), patch.dict(os.environ, env):
                d.transport(self.args)
                d.verify(self.args)
                d.transport(self.args)
        finally:
            for name in names:
                subprocess.run(['docker','rm','-f','-v',name], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)


if __name__ == '__main__':
    unittest.main()
