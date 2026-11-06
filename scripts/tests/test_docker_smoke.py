"""Real public commands, disposable volumes, real GPG/pass, no Tuta account.

Set TUTABRIDGE_DOCKER_IMAGE to a locally built image. Synthetic credentials
are used only against an HTTP fixture; other containers have no network.
"""
import contextlib
import http.server
import json
import os
from pathlib import Path
import subprocess
import tempfile
import threading
import time
import unittest
import uuid

IMAGE = os.environ.get('TUTABRIDGE_DOCKER_IMAGE')
DOCKER = os.environ.get('TUTABRIDGE_DOCKER_BIN', 'docker')
ENTRY = 'Tuta/Bridge/nobody@example.invalid'
SECRET = json.dumps({'login':'nobody@example.invalid', 'user_id':'fakeuser0',
                     'access_token':'fixture-not-a-real-token', 'encrypted_passphrase_key':'AAEC/v8=', 'credential_type':'Internal'})

@unittest.skipUnless(IMAGE, 'set TUTABRIDGE_DOCKER_IMAGE to test a built image')
class DockerSmokeTests(unittest.TestCase):
    def setUp(self):
        self.id = 'tutabridge-smoke-' + uuid.uuid4().hex
        self.volume = self.id + '-data'
        self.containers = []
        self.temp = tempfile.TemporaryDirectory(prefix=self.id)
        self.docker('volume', 'create', self.volume)
        self.network = ['--network', 'none']

    def tearDown(self):
        for name in self.containers:
            self.docker('rm', '--force', '--volumes', name, check=False)
        self.docker('volume', 'rm', self.volume, check=False)
        self.temp.cleanup()

    def docker(self, *args, check=True, input=None, timeout=60):
        result = subprocess.run([DOCKER, *args], input=input, text=True, capture_output=True, timeout=timeout)
        if check: self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        return result

    def run_image(self, *args, entrypoint=None, check=True, input=None, options=()):
        name = self.id + '-run-' + uuid.uuid4().hex[:8]
        self.containers.append(name)
        flags = ['run', '--rm', '--name', name, *self.network, '-v', self.volume + ':/data', *options]
        if entrypoint: flags += ['--entrypoint', entrypoint]
        if input is not None: flags += ['-i']
        result = self.docker(*flags, IMAGE, *args, input=input, check=check)
        self.containers.remove(name)
        return result

    def shell(self, code, **kwargs):
        return self.run_image('-ec', code, entrypoint='/bin/bash', **kwargs)

    def start(self, options=(), args=()):
        name = self.id + '-service-' + uuid.uuid4().hex[:8]
        self.containers.append(name)
        self.docker('run', '-d', '--name', name, *self.network, '-v', self.volume + ':/data', *options, IMAGE, *args)
        return name

    def wait_for(self, name, text, timeout=20):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            logs = self.docker('logs', name)
            if text in logs.stdout + logs.stderr: return
            time.sleep(.1)
        self.fail('state not reached: ' + self.docker('logs', name).stderr)

    def initialize(self, name=None):
        # Empty email cancels before any authentication. Bootstrap still uses
        # the real public CLI and creates real pass/GPG storage under its lock.
        result = (self.docker('exec', '-i', name, 'tutabridge', 'init', input='\n', check=False)
                  if name else self.run_image('init', input='\n', check=False))
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('Email address is required', result.stderr)

    def configure(self, url=None):
        self.shell('sed -i \'s/email = ""/email = "nobody@example.invalid"/\' /data/config/tutabridge/config.toml')
        if url:
            self.shell("printf '\napi_url = \"" + url + "\"\n' >> /data/config/tutabridge/config.toml")

    def insert(self, value):
        self.run_image('insert', '--multiline', '--force', ENTRY, entrypoint='pass', input=value+'\n')

    def checksum(self):
        return self.run_image('/data/pass/'+ENTRY+'.gpg', entrypoint='sha256sum').stdout

    def test_exec_shortcuts_run_the_same_cli_commands(self):
        name = self.start()
        self.wait_for(name, 'Waiting for configuration')
        result = self.docker('exec', '-i', name, 'init', input='\n', check=False)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('Email address is required', result.stderr)
        short = self.docker('exec', name, 'password', check=False)
        full = self.docker('exec', name, 'tutabridge', 'password', check=False)
        self.assertEqual((short.returncode, short.stdout, short.stderr), (full.returncode, full.stdout, full.stderr))

    def test_exec_init_on_empty_volume_and_real_gpg_roundtrip(self):
        name = self.start()
        self.wait_for(name, 'Waiting for configuration')
        self.initialize(name)
        before = self.shell('cat /data/pass/.gpg-id; stat -c "%i" /data/.lock').stdout
        self.insert('fixture-not-a-session')
        self.initialize(name)
        self.assertEqual(before, self.shell('cat /data/pass/.gpg-id; stat -c "%i" /data/.lock').stdout)
        self.assertEqual(self.run_image('show', ENTRY, entrypoint='pass').stdout, 'fixture-not-a-session\n')
        modes = self.shell("stat -c '%a %u' /data/config/tutabridge/config.toml /data/gnupg").stdout.splitlines()
        self.assertEqual(modes, ['600 10001', '700 10001'])

    @unittest.skipUnless(os.name=='posix','PTY requires Unix')
    def test_interactive_exec_init_can_read_its_terminal(self):
        import pty,select
        name=self.start();self.wait_for(name,'Waiting for configuration')
        master,slave=pty.openpty()
        process=subprocess.Popen([DOCKER,'exec','-it',name,'tutabridge','init'],stdin=slave,stdout=slave,stderr=slave)
        os.close(slave)
        try:
            received=b'';deadline=time.monotonic()+10
            while b'Tuta email address:' not in received:
                self.assertLess(time.monotonic(),deadline,'init did not reach terminal prompt')
                if select.select([master],[],[],.1)[0]:received+=os.read(master,8192)
            os.write(master,b'\n')
            self.assertNotEqual(process.wait(timeout=3),0)
            self.docker('exec',name,'flock','-w','3','/data/.lock','true')
        finally:
            if process.poll() is None:process.terminate();process.wait(timeout=3)
            os.close(master)

    def test_waiting_is_unhealthy_without_initialization_and_stops_promptly(self):
        name = self.start()
        self.wait_for(name, 'Waiting for configuration')
        health = self.docker('exec', name, 'tutabridge', 'health', check=False)
        self.assertEqual(health.returncode, 1)
        self.assertIn('Waiting for configuration', health.stderr)
        self.shell('test ! -e /data/config && test ! -e /data/gnupg; flock -w 3 /data/.lock true')
        start = time.monotonic()
        self.docker('stop', '--time', '8', name)
        self.assertLess(time.monotonic()-start, 2)
        self.assertEqual(self.docker('inspect', '-f', '{{.State.ExitCode}}', name).stdout.strip(), '0')

    def test_missing_then_corrupt_session_is_reread_and_preserved(self):
        self.initialize(); self.configure()
        name = self.start()
        self.wait_for(name, 'Waiting for a saved session')
        self.insert('private-invalid-json-never-log-this')
        before = self.checksum()
        self.wait_for(name, 'Session storage unavailable', 15)
        self.assertEqual(before, self.checksum())
        logs = self.docker('logs', name)
        self.assertNotIn('private-invalid-json', logs.stdout + logs.stderr)
        self.assertNotIn('Password for', logs.stdout + logs.stderr)
        failed = self.docker('exec', name, 'tutabridge', 'init', check=False)
        self.assertNotEqual(failed.returncode, 0)
        self.assertEqual(before, self.checksum())

    def test_two_waiters_release_volume_for_init(self):
        names = [self.start(), self.start()]
        for name in names: self.wait_for(name, 'Waiting for configuration')
        self.initialize(names[0]); self.configure()
        for name in names: self.wait_for(name, 'Waiting for a saved session', 15)
        self.shell('flock -w 3 /data/.lock true')

    def test_init_waits_for_observed_lock_ownership(self):
        name = self.id+'-short-owner'; self.containers.append(name)
        self.docker('run','-d','--name',name,'--network','none','-v',self.volume+':/data','--entrypoint','bash',IMAGE,'-c',
                    'exec 9>/data/.lock; flock 9; echo locked; while [ ! -f /tmp/release ]; do sleep .01; done; exec 9>&-; sleep 60')
        self.wait_for(name,'locked')
        process = subprocess.Popen([DOCKER,'exec','-i',name,'tutabridge','init'], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        try:
            # Wait for an actual failed flock attempt, not just process startup.
            import select
            observed=b''; deadline=time.monotonic()+3
            while b'Waiting briefly for volume lock before initialization' not in observed:
                self.assertLess(time.monotonic(),deadline,'init never observed the held lock')
                self.assertIsNone(process.poll(),'init exited instead of waiting')
                if select.select([process.stderr],[],[],.05)[0]:
                    observed+=os.read(process.stderr.fileno(),8192)
            self.docker('exec',name,'bash','-ec','test ! -e /data/config && test ! -e /data/gnupg')
            self.docker('exec',name,'touch','/tmp/release')
            _,stderr=process.communicate('\n',timeout=15)
            self.assertNotEqual(process.returncode,0)
            self.assertIn('Email address is required',stderr)
        finally:
            if process.poll() is None: process.terminate(); process.communicate(timeout=3)

    def test_init_signals_return_failure_and_release_lock(self):
        for sig in ('HUP','QUIT','TERM'):
            with self.subTest(signal=sig):
                name=self.id+'-init-'+sig.lower(); self.containers.append(name)
                try:
                    self.docker('run','-d','--name',name,'--network','none','-v',self.volume+':/data','--entrypoint','bash',IMAGE,'-c',
                                'set -m; mkfifo /tmp/input; exec 3<>/tmp/input; tutabridge init < /tmp/input > /tmp/init-output & echo $! >/tmp/owner; wait $!; echo $? >/tmp/result; sleep 60')
                    deadline=time.monotonic()+15
                    # The CLI flushes this prompt before reading stdin. Unlike
                    # /proc/PID/wchan, it is observable on every runner architecture.
                    while self.docker('exec',name,'grep','-Fq','Tuta email address:','/tmp/init-output',check=False).returncode:
                        self.assertLess(time.monotonic(),deadline,'init did not reach its email prompt');time.sleep(.05)
                    # Job control prevents Bash from imposing SIG_IGN on background QUIT.
                    self.docker('exec',name,'bash','-ec','ignored=$(awk \'/^SigIgn:/ {print $2}\' /proc/$(cat /tmp/owner)/status); test $((16#$ignored & 4)) -eq 0')
                    self.docker('exec',name,'bash','-c','kill -'+sig+' "$(cat /tmp/owner)"')
                    deadline=time.monotonic()+7
                    while self.docker('exec',name,'test','-f','/tmp/result',check=False).returncode:
                        self.assertLess(time.monotonic(),deadline);time.sleep(.05)
                    self.assertNotEqual(self.docker('exec',name,'cat','/tmp/result').stdout.strip(),'0')
                    self.docker('exec',name,'flock','-w','3','/data/.lock','true')
                    logs=self.docker('logs',name)
                    self.assertIn('Initialization failed or cancelled',logs.stderr)
                finally:
                    self.docker('rm','-f','-v',name)
                    self.containers.remove(name)

    @unittest.skipUnless(os.name=='posix','PTY requires Unix')
    def test_ctrl_c_at_init_prompt_releases_lock(self):
        import pty,select
        name=self.start();self.wait_for(name,'Waiting for configuration')
        master,slave=pty.openpty()
        process=subprocess.Popen([DOCKER,'exec','-it',name,'tutabridge','init'],stdin=slave,stdout=slave,stderr=slave)
        os.close(slave)
        try:
            received=b'';deadline=time.monotonic()+15
            while b'Tuta email address:' not in received:
                self.assertLess(time.monotonic(),deadline)
                if select.select([master],[],[],.1)[0]:received+=os.read(master,8192)
            os.write(master,b'\x03')
            self.assertNotEqual(process.wait(timeout=7),0)
            self.docker('exec',name,'flock','-w','3','/data/.lock','true')
        finally:
            if process.poll() is None:process.terminate();process.wait(timeout=3)
            os.close(master)

    def test_spawn_failure_keeps_supervisor_alive(self):
        name=self.id+'-spawn';self.containers.append(name)
        self.docker('run','-d','--name',name,'--network','none','-v',self.volume+':/data','--entrypoint','bash',IMAGE,'-c',
                    'cp /usr/local/bin/tutabridge /tmp/bridge; exec 9>/data/.lock; flock 9; /tmp/bridge run --wait 9>&- & echo $! >/tmp/owner; sleep 1; chmod 600 /tmp/bridge; exec 9>&-; wait $!; echo stopped; sleep 60')
        self.wait_for(name,'Cannot launch worker',15)
        self.docker('exec',name,'bash','-ec','kill -0 "$(cat /tmp/owner)"; flock -w 3 /data/.lock true')
        self.docker('exec',name,'bash','-ec','kill -TERM "$(cat /tmp/owner)"')
        self.wait_for(name,'stopped',3)

    @unittest.skipUnless(os.environ.get('TUTABRIDGE_HEALTH_TEST_BINARY'),'requires compiled Linux unit-test fixture')
    def test_public_health_with_real_servers_and_one_port_down(self):
        fixture=os.environ['TUTABRIDGE_HEALTH_TEST_BINARY']
        name=self.id+'-health';self.containers.append(name)
        self.docker('run','-d','--name',name,'--network','none','-v',self.volume+':/data',
                    '--mount',f'type=bind,src={fixture},dst=/tmp/test-service,readonly',
                    '-e','TUTABRIDGE_PUBLIC_HEALTH_FIXTURE=1','--entrypoint','/tmp/test-service',IMAGE,
                    '--exact','service::tests::health_uses_real_servers_pinned_certificate_and_configured_address','--nocapture')
        self.wait_for(name,'public-health-fixture-ready')
        self.docker('exec',name,'tutabridge','health')
        self.docker('exec',name,'touch','/tmp/tutabridge/stop-smtp')
        self.wait_for(name,'public-health-fixture-smtp-stopped')
        failed=self.docker('exec',name,'tutabridge','health',check=False)
        self.assertNotEqual(failed.returncode,0);self.assertIn('SMTP unavailable',failed.stderr)

    def test_busy_volume_refuses_direct_init_without_bootstrap(self):
        name = self.id + '-owner'; self.containers.append(name)
        self.docker('run', '-d', '--name', name, '--network', 'none', '-v', self.volume+':/data',
                    '--entrypoint', 'bash', IMAGE, '-ec', 'exec 9>/data/.lock; flock -n 9; echo locked; sleep 60')
        self.wait_for(name, 'locked')
        refused = self.docker('exec', name, 'tutabridge', 'init', check=False)
        self.assertNotEqual(refused.returncode, 0)
        self.assertIn('occupied', refused.stderr)
        self.shell('test ! -e /data/gnupg && test ! -e /data/config')

    def test_uid_without_passwd_and_unwritable_volume(self):
        # Test fixture provisioning only; the product never chowns the volume.
        self.shell('touch /data/.fixture; chown 12345:12346 /data', options=('--user','0:0'))
        failed = self.run_image('init', input='\n', options=('--user','12345:12346'), check=False)
        self.assertIn('Email address is required', failed.stderr)
        self.shell('test "$(stat -c %u /data/gnupg)" = 12345', options=('--user','12345:12346'))
        denied = self.run_image('init', options=('--user','12347:12347'), check=False)
        self.assertNotEqual(denied.returncode, 0)
        self.assertIn('permissions', denied.stderr)

    def test_blocked_gpg_is_killed_reaped_before_lock_release(self):
        self.initialize(); self.configure()
        script = Path(self.temp.name)/'gpg'
        script.write_text('#!/bin/bash\ntrap "" TERM INT\necho $$ > /data/blocked-pid\nwhile true; do sleep 60; done\n')
        script.chmod(0o755)
        name = self.id+'-blocked';self.containers.append(name)
        self.docker('run','-d','--name',name,'--network','none','-v',self.volume+':/data',
                    '--mount',f'type=bind,src={script},dst=/usr/local/bin/gpg,readonly',
                    '--entrypoint','bash',IMAGE,'-c',
                    'tutabridge run --wait & echo $! > /tmp/supervisor; wait $!; echo $? > /tmp/result; sleep 60')
        deadline=time.monotonic()+10
        while self.docker('exec',name,'test','-f','/data/blocked-pid',check=False).returncode:
            self.assertLess(time.monotonic(),deadline);time.sleep(.1)
        denied=self.docker('exec',name,'tutabridge','init',check=False)
        self.assertIn('occupied',denied.stderr)
        # Stop the supervisor itself, keeping the namespace alive, so Docker
        # cannot hide surviving children by destroying the entire container.
        self.docker('exec',name,'bash','-ec','kill -TERM "$(cat /tmp/supervisor)"')
        start=time.monotonic()
        while self.docker('exec',name,'test','-f','/tmp/result',check=False).returncode:
            self.assertLess(time.monotonic()-start,7);time.sleep(.05)
        self.assertEqual(self.docker('exec',name,'cat','/tmp/result').stdout.strip(),'0')
        self.docker('exec',name,'bash','-ec','! kill -0 "$(cat /data/blocked-pid)" 2>/dev/null; flock -w 3 /data/.lock true')

    def test_supervisor_reaps_replacement_gpg_agent_without_inherited_lock(self):
        # The product supervisor is PID 1 here: tini and Bash cannot hide a
        # missing reap_agents call by collecting its abandoned daemons for it.
        name=self.start(options=('--entrypoint','/usr/local/bin/tutabridge'), args=('run','--wait'))
        self.wait_for(name,'Waiting for configuration'); self.initialize(name)
        self.configure(); self.insert('fixture-invalid-json'); self.wait_for(name,'Session storage unavailable')
        self.docker('exec',name,'gpgconf','--kill','gpg-agent')
        time.sleep(12)
        self.docker('exec',name,'bash','-ec','''test "$(cat /proc/1/comm)" = tutabridge
            : > /tmp/agent-pids
            for process in /proc/[0-9]*; do
                [ "$(cat "$process/comm" 2>/dev/null || true)" = gpg-agent ] || continue
                [ "$(awk '{print $3}' "$process/stat")" != Z ] || exit 1
                [ "$(awk '/^PPid:/ {print $2}' "$process/status")" = 1 ] || exit 1
                echo "${process##*/}" >> /tmp/agent-pids
                for fd in "$process"/fd/*; do
                    [ "$(readlink "$fd" 2>/dev/null || true)" != /data/.lock ] || exit 1
                done
            done
            test -s /tmp/agent-pids
            flock -w 3 /data/.lock true
        ''')
        self.docker('exec',name,'gpgconf','--kill','gpg-agent')
        deadline=time.monotonic()+4
        while self.docker('exec',name,'bash','-ec',
                'while read -r pid; do test ! -d "/proc/$pid" || exit 1; done < /tmp/agent-pids',check=False).returncode:
            self.assertLess(time.monotonic(),deadline,'supervisor left a dead GPG agent unreaped')
            time.sleep(.1)

    @contextlib.contextmanager
    def server(self, status):
        counts=[]
        class Handler(http.server.BaseHTTPRequestHandler):
            def do_GET(self):
                counts.append(self.path)
                self.send_response(status);self.send_header('Content-Length','0');self.end_headers()
            do_POST=do_GET
            def log_message(self,*args): pass
        server=http.server.ThreadingHTTPServer(('0.0.0.0',0),Handler)
        thread=threading.Thread(target=server.serve_forever,daemon=True);thread.start()
        desktop='Docker Desktop' in self.docker('info','--format','{{.OperatingSystem}}').stdout
        self.network=['--network','bridge'] if desktop else ['--network','host']
        host='host.docker.internal' if desktop else '127.0.0.1'
        try: yield f'http://{host}:{server.server_port}',counts
        finally: server.shutdown();server.server_close();thread.join()

    def failure_is_preserved(self, status, reason):
        self.initialize()
        with self.server(status) as (url,counts):
            self.configure(url);self.insert(SECRET)
            before=self.checksum();name=self.start()
            self.wait_for(name,reason)
            self.assertTrue(counts,'fixture was not requested')
            total=len(counts);time.sleep(11)
            self.assertEqual(len(counts),total,'permanent failure retried without repair')
            self.assertEqual(before,self.checksum())
            self.assertNotEqual(self.docker('exec',name,'tutabridge','health',check=False).returncode,0)
            failed=self.docker('exec',name,'tutabridge','init',check=False)
            self.assertNotEqual(failed.returncode,0)
            self.assertEqual(before,self.checksum(),'failed init changed saved bytes')

    def test_rejected_decodable_session_is_kept_and_not_retried(self):
        self.failure_is_preserved(401,'Saved session rejected')

    def test_474_waits_for_new_image(self):
        self.failure_is_preserved(474,'Tuta rejects this version')

    def test_health_in_another_container_cannot_claim_the_others_status(self):
        name=self.start();self.wait_for(name,'Waiting for configuration')
        result=self.run_image('health',check=False)
        self.assertNotEqual(result.returncode,0)
        self.assertIn('Service status unavailable',result.stderr)

    def test_internal_worker_cannot_bypass_lock(self):
        result=self.run_image('__container-init',check=False)
        self.assertNotEqual(result.returncode,0)
        self.assertIn('inherited volume lock',result.stderr)
        self.shell('test ! -e /data/gnupg')

if __name__=='__main__': unittest.main()
