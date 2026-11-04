"""Opt-in image tests using disposable volumes and real pass/GnuPG.

TUTABRIDGE_DOCKER_IMAGE=tutabridge:test python3 -m unittest discover \
    -s scripts/tests -p test_docker_smoke.py -v
No account, network login, existing volume or host keyring is used.
"""
import os
from pathlib import Path
import subprocess
import tempfile
import time
import unittest
import uuid

IMAGE = os.environ.get("TUTABRIDGE_DOCKER_IMAGE")
DOCKER = os.environ.get("TUTABRIDGE_DOCKER_BIN", "docker")
FAKE_CLI = '''#!/bin/bash
set -eu
case "$1" in
  init) exit 0 ;;
  run)
    trap 'exit 0' TERM INT
    echo smoke-ready
    while true; do
      if [[ -e /data/smoke-load ]]; then
        # This is the bridge process itself, so the real pass child inherits
        # the same descriptor environment as a CLI-initiated session read.
        readlink "/proc/$$/fd/9" > /data/smoke-lock
        pass show Tuta/Bridge/agent-restart@example.invalid > /dev/null
        rm /data/smoke-load
        touch /data/smoke-loaded
      fi
      sleep 0.1 & wait "$!"
    done
    ;;
  *) exit 2 ;;
esac
'''


@unittest.skipUnless(IMAGE, "set TUTABRIDGE_DOCKER_IMAGE to test a built image")
class DockerSmokeTests(unittest.TestCase):
    def setUp(self):
        self.id = "tutabridge-smoke-" + uuid.uuid4().hex
        self.volume = self.id + "-data"
        self.containers = []
        self.temp = tempfile.TemporaryDirectory(prefix=self.id)
        self.fake_cli = Path(self.temp.name) / "tutabridge"
        self.fake_cli.write_text(FAKE_CLI)
        self.fake_cli.chmod(0o755)
        self.docker("volume", "create", self.volume)

    def tearDown(self):
        for name in self.containers:
            self.docker("rm", "--force", name, check=False)
        self.docker("volume", "rm", self.volume, check=False)
        self.temp.cleanup()

    def docker(self, *args, check=True, input=None, timeout=60):
        result = subprocess.run([DOCKER, *args], input=input, text=True,
                                capture_output=True, timeout=timeout)
        if check:
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        return result

    def run_image(self, *args, fake=False, entrypoint=None, check=True, input=None):
        # Keep a name until completion so tearDown can remove a container
        # even when the docker client times out or an assertion fails.
        name = self.id + "-run-" + uuid.uuid4().hex[:8]
        self.containers.append(name)
        options = ["run", "--rm", "--name", name, "--volume", self.volume + ":/data"]
        if fake:
            options += ["--mount", f"type=bind,src={self.fake_cli},dst=/usr/local/bin/tutabridge,readonly"]
        if entrypoint:
            options += ["--entrypoint", entrypoint]
        if input is not None:
            options += ["--interactive"]
        result = self.docker(*options, IMAGE, *args, check=check, input=input)
        self.containers.remove(name)
        return result

    def initialize(self):
        # Isolate container provisioning from authentication. Other checks use
        # the actual CLI; this stand-in never creates a fake Tuta session.
        self.run_image("init", fake=True)

    def configure_email(self):
        self.run_image("-c", "sed -i 's/email = \"\"/email = \"nobody@example.invalid\"/' /data/config/tutabridge/config.toml",
                       entrypoint="/bin/sh")

    def test_real_gpg_roundtrip_survives_recreation_and_init_is_idempotent(self):
        self.initialize()
        before = self.run_image("/data/pass/.gpg-id", entrypoint="cat").stdout
        self.run_image("insert", "--multiline", "--force", "Tuta/Bridge/test@example.invalid",
                       entrypoint="pass", input="a test value, not a Tuta session\n")
        self.run_image("init", fake=True)
        after = self.run_image("/data/pass/.gpg-id", entrypoint="cat").stdout
        self.assertEqual(before, after)
        result = self.run_image("show", "Tuta/Bridge/test@example.invalid", entrypoint="pass")
        self.assertEqual(result.stdout, "a test value, not a Tuta session\n")
        modes = self.run_image("-c", "stat -c '%a %u' /data/config/tutabridge/config.toml /data/gnupg",
                               entrypoint="/bin/sh").stdout.splitlines()
        self.assertEqual(modes, ["600 10001", "700 10001"])

    def test_empty_volume_fails_without_prompt_or_generation(self):
        result = self.run_image("run", check=False)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("run --rm tutabridge init", result.stderr)
        self.run_image("-c", "test ! -e /data/gnupg && test ! -e /data/config", entrypoint="/bin/sh")
        self.assertNotIn("Password for", result.stdout + result.stderr)

    def test_real_cli_rejects_missing_and_corrupt_sessions_without_prompt(self):
        self.initialize()
        self.configure_email()
        missing = self.run_image("run", check=False)
        self.assertNotEqual(missing.returncode, 0)
        self.assertNotIn("Password for", missing.stdout + missing.stderr)
        self.run_image("insert", "--multiline", "--force", "Tuta/Bridge/nobody@example.invalid",
                       entrypoint="pass", input="secret-invalid-json-never-log-this\n")
        corrupt = self.run_image("run", check=False)
        self.assertNotEqual(corrupt.returncode, 0)
        self.assertIn("invalid session", corrupt.stderr.lower())
        self.assertNotIn("secret-invalid-json-never-log-this", corrupt.stdout + corrupt.stderr)
        self.assertNotIn("Password for", corrupt.stdout + corrupt.stderr)
        # A failed read must leave the encrypted entry in place.
        self.run_image("-c", "test -s /data/pass/Tuta/Bridge/nobody@example.invalid.gpg", entrypoint="/bin/sh")

    def test_init_without_terminal_explains_recovery_and_preserves_corrupt_entry(self):
        self.initialize()
        self.configure_email()
        entry = "Tuta/Bridge/nobody@example.invalid"
        self.run_image("insert", "--multiline", "--force", entry,
                       entrypoint="pass", input="secret-invalid-json-never-log-this\n")
        before = self.run_image("/data/pass/" + entry + ".gpg", entrypoint="sha256sum").stdout
        # run_image does not allocate a TTY, even when stdin is supplied.
        result = self.run_image("init", check=False)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("invalid session", result.stderr)
        self.assertIn("interactive terminal is required", result.stderr)
        self.assertIn("docker compose run --rm tutabridge init", result.stderr)
        self.assertNotIn("secret-invalid-json-never-log-this", result.stdout + result.stderr)
        after = self.run_image("/data/pass/" + entry + ".gpg", entrypoint="sha256sum").stdout
        self.assertEqual(before, after)

    def test_lock_agent_descriptors_and_sigterm(self):
        self.initialize()
        self.run_image("insert", "--multiline", "--force", "Tuta/Bridge/agent-restart@example.invalid",
                       entrypoint="pass", input="agent restart test value\n")
        name = self.id + "-running"
        self.containers.append(name)
        self.docker("run", "--detach", "--name", name, "--volume", self.volume + ":/data",
                    "--mount", f"type=bind,src={self.fake_cli},dst=/usr/local/bin/tutabridge,readonly",
                    IMAGE, "run")
        deadline = time.monotonic() + 10
        while time.monotonic() < deadline:
            if "smoke-ready" in self.docker("logs", name).stdout:
                break
            time.sleep(0.1)
        else:
            self.fail("container did not start: " + self.docker("logs", name).stderr)
        for command in ("init", "run"):
            result = self.run_image(command, fake=True, check=False)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("already in use", result.stderr)
        before = self.docker("exec", name, "gpg-connect-agent", "GETINFO pid", "/bye").stdout
        old_pid = next(line[2:] for line in before.splitlines() if line.startswith("D "))
        self.assertTrue(old_pid.isdigit(), before)
        # Simulate an agent crash, including its stale socket, not just a
        # graceful shutdown performed by gpgconf.
        self.docker("exec", name, "/bin/sh", "-c", 'kill -KILL "$1"', "sh", old_pid)
        self.docker("exec", name, "touch", "/data/smoke-load")
        deadline = time.monotonic() + 10
        while time.monotonic() < deadline:
            if self.docker("exec", name, "test", "-e", "/data/smoke-loaded", check=False).returncode == 0:
                break
            time.sleep(0.1)
        else:
            self.fail("the bridge's pass subprocess did not reload: " + self.docker("logs", name).stderr)
        self.assertEqual(self.docker("exec", name, "cat", "/data/smoke-lock").stdout.strip(), "/data/.lock")
        after = self.docker("exec", name, "gpg-connect-agent", "GETINFO pid", "/bye").stdout
        self.assertNotEqual(before, after, "pass should have launched a replacement agent")
        # Check the replacement real gpg-agent has not inherited the volume
        # lock after pass relaunched it from the process holding descriptor 9.
        self.docker("exec", name, "/bin/sh", "-ec", '''
            found=0
            for process in /proc/[0-9]*; do
                [ "$(cat "$process/comm" 2>/dev/null || true)" = gpg-agent ] || continue
                found=1
                for fd in "$process"/fd/*; do
                    [ "$(readlink "$fd" 2>/dev/null || true)" != /data/.lock ] || exit 1
                done
            done
            test "$found" = 1
        ''')
        self.docker("stop", "--time", "5", name)
        code = self.docker("inspect", "--format", "{{.State.ExitCode}}", name).stdout.strip()
        self.assertEqual(code, "0", "SIGTERM was not handled before Docker's kill timeout")
        self.run_image("init", fake=True)


if __name__ == "__main__":
    unittest.main()
