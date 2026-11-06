"""Exercise the native build script with disposable command doubles, no daemon."""
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]
WORKFLOW = ROOT / '.github/workflows/docker-build.yml'


@unittest.skipUnless(shutil.which('bash'), 'workflow requires bash')
class NativeImageWorkflow(unittest.TestCase):
    def run_workflow(self, arch='arm64', modern=True, failure='', directory=None):
        root = Path(directory or self.root.name)
        source = root / 'source'
        (source / 'scripts/tests').mkdir(parents=True)
        (source / 'scripts/sdk-prepare.py').write_text('')
        (source / 'docker/tests').mkdir(parents=True)
        if modern and failure != 'missing-harness':
            (source / 'docker/tests/run-real-pass.sh').write_text('# fixture harness\n')
        smoke = '''import os, unittest
from pathlib import Path
class Smoke(unittest.TestCase):
    def test_image(self):
        self.assertEqual(os.environ.get('TUTABRIDGE_DOCKER_IMAGE'), 'tutabridge:test')
'''
        if modern:
            smoke += '''    @unittest.skipUnless(os.environ.get('TUTABRIDGE_HEALTH_TEST_BINARY'), 'requires compiled Linux unit-test fixture')
    def test_health(self):
        self.assertTrue(Path(os.environ['TUTABRIDGE_HEALTH_TEST_BINARY']).is_file())
'''
        if failure == 'empty-suite':
            smoke = ''
        elif failure == 'smoke':
            smoke += '    def test_failure(self): self.fail("smoke failed")\n'
        (source / 'scripts/tests/test_docker_smoke.py').write_text(smoke)
        automation = root / 'automation/scripts'
        automation.mkdir(parents=True)
        (automation / 'install-regctl.sh').write_text('exit 0\n')
        for name in ('docker-smoke.py', 'build-docker-image.sh'):
            shutil.copy2(ROOT / 'scripts' / name, automation)
        binary = root / 'bin'
        binary.mkdir()
        commands = '''import hashlib, json, os, pathlib, sys
args = sys.argv[1:]
command = pathlib.Path(sys.argv[0]).name
with open(os.environ['COMMAND_LOG'], 'a') as log:
    log.write(json.dumps([command, args, os.getcwd()])+'\\n')
failure = os.environ['FAILURE']
if command == 'sha256sum':
    print(hashlib.sha256(pathlib.Path(args[0]).read_bytes()).hexdigest()+'  '+args[0])
elif command == 'docker':
    if args[:2] == ['buildx', 'build']:
        if '--target' in args:
            if failure == 'fixture-build': sys.exit(9)
            destination = pathlib.Path(args[args.index('--output')+1].split('dest=',1)[1])
            destination.mkdir(parents=True)
            for name in ('test-service', 'test-core'):
                if failure == 'missing-'+name: continue
                fixture = destination/name
                fixture.write_text('#!/bin/sh\\nexit 0\\n')
                fixture.chmod(0o755)
        else:
            pathlib.Path('image.tar').write_bytes(b'exported OCI archive')
    elif args == ['load']:
        sys.stdin.read()
    elif '--version' in args:
        print('tutabridge '+os.environ['VERSION'])
    elif 'service::lifecycle_tests' in args:
        if failure == 'lifecycle': sys.exit(10)
    elif '/tmp/run-real-pass.sh' in args:
        if failure == 'real-pass': sys.exit(11)
        if failure == 'changed-archive': pathlib.Path('image.tar').write_bytes(b'changed')
    else:
        raise AssertionError(args)
elif command == 'regctl':
    if args[:2] == ['image', 'export']: print('docker import stream')
else:
    raise AssertionError(command)
'''
        for name in ('docker', 'regctl', 'sha256sum'):
            executable = binary / name
            executable.write_text('#!' + sys.executable + '\n' + commands)
            executable.chmod(0o755)
        (binary / 'python3').symlink_to(sys.executable)
        output = root / 'output'
        env = dict(os.environ, PATH=str(binary)+os.pathsep+os.environ['PATH'],
                   VERSION='0.1.0-rc.99', SOURCE='a'*40, CREATED='2026-11-06T00:00:00Z',
                   ARCH=arch, RUNNER_TEMP=str(root/'temp'), GITHUB_WORKSPACE=str(root),
                   GITHUB_OUTPUT=str(output), COMMAND_LOG=str(root/'commands'), FAILURE=failure,
                   PYTHONDONTWRITEBYTECODE='1')
        env.pop('TUTABRIDGE_HEALTH_TEST_BINARY', None)
        result = subprocess.run(['bash', str(automation / 'build-docker-image.sh')],
                                cwd=root, env=env, capture_output=True, text=True)
        commands = [json.loads(line) for line in (root/'commands').read_text().splitlines()]
        return result, output, commands

    def setUp(self):
        self.root = tempfile.TemporaryDirectory()
        self.addCleanup(self.root.cleanup)

    def test_workflow_calls_the_tested_script(self):
        self.assertIn('run: bash automation/scripts/build-docker-image.sh', WORKFLOW.read_text())

    def test_each_native_arch_builds_same_source_fixtures_before_smoke(self):
        for arch in ('amd64', 'arm64'):
            with self.subTest(arch=arch), tempfile.TemporaryDirectory() as directory:
                result, output, commands = self.run_workflow(arch, directory=directory)
                self.assertEqual(result.returncode, 0, result.stdout+result.stderr)
                self.assertIn('Ran 2 tests', result.stderr)
                self.assertNotIn('skipped', result.stderr)
                self.assertEqual(output.read_text(), 'sha256='+hashlib.sha256(b'exported OCI archive').hexdigest()+'\n')
                builds = [(args, cwd) for tool, args, cwd in commands if tool == 'docker' and args[:2] == ['buildx', 'build']]
                self.assertEqual(len(builds), 2)
                for args, cwd in builds:
                    self.assertEqual(args[args.index('--platform')+1], 'linux/'+arch)
                    self.assertIn('TUTABRIDGE_VERSION=0.1.0-rc.99', args)
                    self.assertIn('TUTABRIDGE_REVISION='+'a'*40, args)
                    self.assertIn('TUTABRIDGE_CREATED=2026-11-06T00:00:00Z', args)
                    self.assertEqual(Path(cwd), (Path(directory)/'source').resolve())
                self.assertEqual(builds[1][0][builds[1][0].index('--target')+1], 'test-binaries')
                fixtures = [args for tool, args, _ in commands if tool == 'docker' and '--entrypoint' in args]
                self.assertEqual(len(fixtures), 2)
                for args in fixtures:
                    self.assertIn('none', args)
                    self.assertIn('tutabridge:test', args)
                self.assertIn('service::lifecycle_tests', fixtures[0])
                self.assertEqual(fixtures[1][-1], '/tmp/run-real-pass.sh')
                self.assertIn(f'type=bind,src={directory}/source/docker/tests/run-real-pass.sh,dst=/tmp/run-real-pass.sh,readonly', fixtures[1])

    def test_legacy_source_runs_its_suite_without_a_fixture_stage(self):
        result, output, commands = self.run_workflow(modern=False)
        self.assertEqual(result.returncode, 0, result.stdout+result.stderr)
        self.assertTrue(output.exists())
        self.assertIn('Ran 1 test', result.stderr)
        self.assertFalse(any('--target' in args for _, args, _ in commands))

    def test_missing_fixtures_skipped_tests_or_failed_checks_never_export_digest(self):
        for failure in ('fixture-build', 'missing-test-core', 'missing-test-service', 'missing-harness',
                        'lifecycle', 'real-pass', 'smoke', 'empty-suite', 'changed-archive'):
            with self.subTest(failure=failure), tempfile.TemporaryDirectory() as directory:
                result, output, _ = self.run_workflow(failure=failure, directory=directory)
                self.assertNotEqual(result.returncode, 0, result.stdout+result.stderr)
                self.assertFalse(output.exists(), failure)
                if failure in ('missing-harness', 'empty-suite'):
                    self.assertIn('requires a nonempty suite with no skipped tests', result.stderr)
