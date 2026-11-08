"""Container orchestration tests with private executables and no Tuta account."""
import json
import os
from pathlib import Path
import signal
import shutil
import subprocess
import sys
import tempfile
import time
import unittest

ROOT = Path(__file__).resolve().parents[2]
ENTRYPOINT = ROOT / "docker/bootstrap.sh"
FINGERPRINT = "A" * 40

STUB = r'''
import fcntl, json, os, pathlib, signal, sys, time
root = pathlib.Path(os.environ["DOCKER_TEST_ROOT"])
name = pathlib.Path(sys.argv[0]).name
args = sys.argv[1:]
with (root / "calls").open("a") as log:
    log.write(json.dumps([name, args]) + "\n")
if name == "flock":
    try:
        fcntl.flock(int(args[-1]), fcntl.LOCK_EX | fcntl.LOCK_NB)
    except BlockingIOError:
        sys.exit(1)
elif name in ("gpg", "gpgconf", "pass"):
    # Descriptor inheritance is exercised through the real supervisor in test_docker_smoke.py.
    if name == "gpg":
        if "--list-secret-keys" in args:
            if (root / "keys").exists():
                for key in (root / "keys").read_text().splitlines():
                    print("sec:::::::::")
                    print("fpr:::::::::" + key + ":")
                    print("ssb:::::::::")
                    print("fpr:::::::::" + "B" * 40 + ":")
        elif "--generate-key" in args:
            (root / "keys").write_text("A" * 40 + "\n")
        else:
            sys.exit("unexpected gpg command")
    elif name == "pass":
        assert args[0] == "init"
        pathlib.Path(os.environ["PASSWORD_STORE_DIR"], ".gpg-id").write_text(args[1] + "\n")
elif name == "tutabridge":
    if args == ["--version"]:
        print("tutabridge test")
    elif os.environ.get("DOCKER_TEST_WAIT") == "1":
        assert args == ["run", "--non-interactive"]
        assert os.environ["PASSWORD_STORE_GPG_OPTS"] == "--batch --pinentry-mode error"
        def stop(sig, frame):
            (root / "stopped").write_text(str(sig))
            sys.exit(0)
        signal.signal(signal.SIGTERM, stop)
        (root / "started").touch()
        while True:
            time.sleep(1)
    sys.exit(int(os.environ.get("DOCKER_TEST_EXIT", "0")))
else:
    sys.exit("unexpected executable")
'''


@unittest.skipUnless(os.name == "posix", "container entrypoint requires Unix")
class DockerEntrypointTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="tutabridge-docker-test-")
        self.root = Path(self.temp.name)
        self.bin = self.root / "bin"
        self.bin.mkdir()
        # Expose only required utilities, never a host pass or GPG executable.
        for name in ("bash", "dirname", "awk", "cat", "chmod", "cp", "find", "mkdir"):
            executable = shutil.which(name)
            if executable is None:
                self.skipTest(f"required utility unavailable: {name}")
            (self.bin / name).symlink_to(executable)
        for name in ("gpg", "gpgconf", "pass", "flock", "tutabridge"):
            path = self.bin / name
            path.write_text(f"#!{sys.executable}\n" + STUB)
            path.chmod(0o700)
        self.data = self.root / "data"
        self.config = self.data / "config/tutabridge/config.toml"
        self.env = dict(os.environ, DOCKER_TEST_ROOT=str(self.root),
                        TUTABRIDGE_DATA_DIR=str(self.data),
                        XDG_CONFIG_HOME=str(self.data / "config"),
                        XDG_CACHE_HOME=str(self.data / "cache"),
                        GNUPGHOME=str(self.data / "gnupg"),
                        PASSWORD_STORE_DIR=str(self.data / "pass"),
                        PATH=str(self.bin))

    def tearDown(self):
        self.temp.cleanup()

    def run_entrypoint(self, *args, **extra_env):
        return subprocess.run(["bash", str(ENTRYPOINT), *args],
                              env=dict(self.env, **extra_env),
                              text=True, capture_output=True, timeout=30)

    def calls(self, name):
        path = self.root / "calls"
        if not path.exists():
            return []
        return [args for executable, args in
                map(json.loads, path.read_text().splitlines()) if executable == name]

    def assert_ok(self, result):
        self.assertEqual(result.returncode, 0, result.stderr)

    def initialized(self):
        self.assert_ok(self.run_entrypoint("init"))

    def test_fresh_init_and_retry_preserve_key_config_and_entries(self):
        self.initialized()
        self.assertIn('pass_path = "Tuta/Bridge"', self.config.read_text())
        self.assertEqual(self.config.stat().st_mode & 0o777, 0o600)
        self.config.write_text("user's existing configuration")
        secret = self.data / "pass/session.gpg"
        secret.write_text("existing encrypted session")
        self.assert_ok(self.run_entrypoint("init"))
        self.assertEqual(self.config.read_text(), "user's existing configuration")
        self.assertEqual(secret.read_text(), "existing encrypted session")
        self.assertEqual(len([a for a in self.calls("gpg") if "--generate-key" in a]), 1)
        self.assertEqual(self.calls("pass"), [["init", FINGERPRINT]])
        self.assertEqual(self.calls("tutabridge"), [])

    def test_interrupted_generation_reuses_existing_key(self):
        (self.root / "keys").write_text(FINGERPRINT + "\n")
        self.initialized()
        self.assertFalse(any("--generate-key" in a for a in self.calls("gpg")))
        self.assertEqual(self.calls("pass"), [["init", FINGERPRINT]])

    def test_existing_data_without_recipient_is_never_reencrypted(self):
        store = self.data / "pass"
        store.mkdir(parents=True)
        (store / "session.gpg").write_text("keep")
        result = self.run_entrypoint("init")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn(".gpg-id is missing", result.stderr)
        self.assertEqual((store / "session.gpg").read_text(), "keep")
        self.assertEqual(self.calls("gpg"), [])
        self.assertEqual(self.calls("pass"), [])

    def test_missing_private_key_keeps_existing_recipient(self):
        self.initialized()
        (self.root / "keys").unlink()
        result = self.run_entrypoint("init")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("private GPG key", result.stderr)
        self.assertEqual((self.data / "pass/.gpg-id").read_text(), FINGERPRINT + "\n")
        self.assertEqual(len(self.calls("pass")), 1)

    def test_ambiguous_keys_require_explicit_recovery(self):
        (self.root / "keys").write_text(FINGERPRINT + "\n" + "C" * 40 + "\n")
        result = self.run_entrypoint("init")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("exactly one", result.stderr)
        self.assertEqual(self.calls("pass"), [])

    def test_invalid_recipient_is_not_overwritten(self):
        self.initialized()
        recipient = self.data / "pass/.gpg-id"
        recipient.write_text("unexpected\n")
        result = self.run_entrypoint("init")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Unexpected pass recipient", result.stderr)
        self.assertEqual(recipient.read_text(), "unexpected\n")

    def test_run_on_empty_volume_does_not_initialize(self):
        result = self.run_entrypoint("check")
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(result.returncode, 10)
        self.assertFalse(self.config.exists())
        self.assertFalse((self.data / "gnupg").exists())
        self.assertEqual(self.calls("pass"), [])
        self.assertEqual(self.calls("tutabridge"), [])

    def test_missing_pass_fails_before_authentication(self):
        (self.bin / "pass").unlink()
        result = self.run_entrypoint("init")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("pass is unavailable", result.stderr)
        self.assertEqual(self.calls("tutabridge"), [])

    def test_launcher_defaults_to_wait_and_never_touches_volume(self):
        for args, expected in [((), ["run", "--wait"]), (("run",), ["run", "--wait"]),
                               (("init",), ["init"]), (("health",), ["health"]),
                               (("run", "--non-interactive"), ["run", "--non-interactive"])]:
            result = subprocess.run(["bash", str(ROOT / "docker/entrypoint.sh"), *args],
                                    env=self.env, capture_output=True, text=True, timeout=5)
            self.assert_ok(result)
            self.assertEqual(self.calls("tutabridge")[-1], expected)
        self.assertFalse(self.data.exists())


if __name__ == "__main__":
    unittest.main()
