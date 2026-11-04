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
ENTRYPOINT = ROOT / "docker/entrypoint.sh"
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
    try:
        os.fstat(9)
    except OSError:
        pass
    else:
        sys.exit("GPG setup inherited the volume lock")
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
        self.assertEqual(self.calls("tutabridge"), [["init"], ["init"]])

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
        result = self.run_entrypoint("run")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("run --rm tutabridge init", result.stderr)
        self.assertFalse(self.config.exists())
        self.assertFalse((self.data / "gnupg").exists())
        self.assertEqual(self.calls("pass"), [])
        self.assertEqual(self.calls("tutabridge"), [])

    def test_bridge_exit_status_is_preserved(self):
        self.initialized()
        result = self.run_entrypoint("run", DOCKER_TEST_EXIT="23")
        self.assertEqual(result.returncode, 23)
        self.assertEqual(self.calls("tutabridge")[-1], ["run", "--non-interactive"])

    def test_missing_pass_fails_before_authentication(self):
        (self.bin / "pass").unlink()
        result = self.run_entrypoint("init")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("pass is unavailable", result.stderr)
        self.assertEqual(self.calls("tutabridge"), [])

    def test_help_and_unknown_commands_do_not_touch_volume(self):
        self.assert_ok(self.run_entrypoint("--version"))
        self.assertFalse(self.data.exists())
        result = self.run_entrypoint("typo")
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse(self.data.exists())

    def test_lock_excludes_init_and_run_and_sigterm_releases_it(self):
        self.initialized()
        process = subprocess.Popen(["bash", str(ENTRYPOINT), "run"],
                                   env=dict(self.env, DOCKER_TEST_WAIT="1"),
                                   stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, text=True)
        try:
            deadline = time.monotonic() + 5
            while not (self.root / "started").exists() and time.monotonic() < deadline:
                if process.poll() is not None:
                    self.fail(process.stderr.read())
                time.sleep(0.01)
            self.assertTrue((self.root / "started").exists())
            for command in ("init", "run"):
                result = self.run_entrypoint(command)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("already in use", result.stderr)
            process.send_signal(signal.SIGTERM)
            self.assertEqual(process.wait(timeout=5), 0)
            self.assertEqual((self.root / "stopped").read_text(), str(signal.SIGTERM))
            self.assert_ok(self.run_entrypoint("init"))
        finally:
            if process.poll() is None:
                process.kill()
                process.wait(timeout=5)
            process.stderr.close()


if __name__ == "__main__":
    unittest.main()
