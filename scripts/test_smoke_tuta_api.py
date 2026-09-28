"""Offline regression tests for the network smoke check."""

from contextlib import redirect_stderr, redirect_stdout
import io
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import MagicMock, patch
import urllib.error

import smoke_tuta_api as smoke


def response(status):
    result = MagicMock()
    result.__enter__.return_value.status = status
    return result


def http_error(status):
    return urllib.error.HTTPError(smoke.ENDPOINT, status, "test", {}, None)


class SmokeTest(unittest.TestCase):
    def run_probe(self, replies):
        output = io.StringIO()
        with (
            patch.object(smoke.urllib.request, "build_opener") as build,
            patch.object(smoke.time, "sleep") as sleep,
            redirect_stdout(output),
            redirect_stderr(output),
        ):
            open_request = build.return_value.open
            open_request.side_effect = replies
            result = smoke.check_version("359.260904.0", "2")
        return result, output.getvalue(), open_request, sleep

    def test_success_sends_only_version_headers_without_credentials(self):
        result, _, calls, sleep = self.run_probe([response(200)])
        self.assertEqual(result, 0)
        request = calls.call_args.args[0]
        self.assertEqual(request.full_url, smoke.ENDPOINT)
        self.assertEqual(request.get_method(), "GET")
        self.assertEqual(dict(request.header_items()), {"Cv": "359.260904.0", "V": "2"})
        self.assertIsNone(request.data)
        self.assertEqual(calls.call_args.kwargs["timeout"], 15)
        sleep.assert_not_called()

    def test_version_rejection_fails_immediately(self):
        result, output, calls, sleep = self.run_probe([http_error(474)])
        self.assertEqual(result, 1)
        self.assertIn("HTTP 474 Invalid Software Version", output)
        self.assertEqual(calls.call_count, 1)
        sleep.assert_not_called()

    def test_transient_errors_can_recover(self):
        for error in (urllib.error.URLError("offline"), TimeoutError(), http_error(429), http_error(503)):
            with self.subTest(error=error):
                result, _, calls, sleep = self.run_probe([error, response(200)])
                self.assertEqual(result, 0)
                self.assertEqual(calls.call_count, 2)
                sleep.assert_called_once_with(2)

    def test_persistent_outage_is_inconclusive_and_bounded(self):
        result, output, calls, sleep = self.run_probe([http_error(503) for _ in range(3)])
        self.assertEqual(result, 2)
        self.assertIn("INCONCLUSIVE", output)
        self.assertEqual(calls.call_count, 3)
        self.assertEqual(sleep.call_count, 2)

    def test_other_statuses_are_not_false_successes(self):
        for status in (204, 301, 302, 401, 403, 404):
            with self.subTest(status=status):
                reply = response(status) if status < 300 else http_error(status)
                result, output, calls, sleep = self.run_probe([reply])
                self.assertEqual(result, 2)
                self.assertIn(f"Unexpected HTTP {status}", output)
                self.assertEqual(calls.call_count, 1)
                sleep.assert_not_called()
        self.assertIsNone(smoke.NoRedirect().redirect_request(None, None, 302, "", {}, "https://example.test"))

    def test_versions_come_from_the_sdk_not_a_hardcoded_value(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            sdk = root / "tuta-repo/tuta-sdk/rust/sdk"
            (sdk / "src/type_models").mkdir(parents=True)
            (sdk / "Cargo.toml").write_text("[package]\nversion.workspace = true\n")
            (root / "tuta-repo/Cargo.toml").write_text('[workspace.package]\nversion = "400.1.0"\n')
            (sdk / "src/type_models/base.json").write_text(json.dumps({"0": {"version": 9}}))
            self.assertEqual(smoke.sdk_versions(root), ("400.1.0", "9"))
            (sdk / "Cargo.toml").write_text('[package]\nversion = "401.2.0"\n')
            self.assertEqual(smoke.sdk_versions(root), ("401.2.0", "9"))


if __name__ == "__main__":
    unittest.main()
