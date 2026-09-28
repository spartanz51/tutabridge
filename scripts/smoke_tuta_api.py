#!/usr/bin/env python3
"""Check Tuta's client-version gate without credentials (Python 3.11+)."""

import json
from pathlib import Path
import sys
import time
import tomllib
import urllib.error
import urllib.request


ENDPOINT = "https://app.tuta.com/rest/base/applicationtypesservice"
ROOT = Path(__file__).resolve().parents[1]


def sdk_versions(root=ROOT):
    sdk = root / "tuta-repo/tuta-sdk/rust/sdk"
    with (sdk / "Cargo.toml").open("rb") as manifest:
        version = tomllib.load(manifest)["package"]["version"]
    if isinstance(version, dict) and version.get("workspace") is True:
        with (root / "tuta-repo/Cargo.toml").open("rb") as manifest:
            version = tomllib.load(manifest)["workspace"]["package"]["version"]
    if not isinstance(version, str) or not version:
        raise ValueError("Cannot determine the pinned SDK package version")
    models = json.loads((sdk / "src/type_models/base.json").read_text())
    # Match read_type_models! in the SDK: every type in an app has one version.
    model_versions = {str(model["version"]) for model in models.values()}
    if len(model_versions) != 1:
        raise ValueError("Expected one base-model version in the pinned SDK")
    return version, model_versions.pop()


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        # A redirected HTML page must never count as an accepted API request.
        return None


def check_version(client_version, model_version):
    """Return 0 for acceptance, 1 for HTTP 474, 2 for an inconclusive check."""
    request = urllib.request.Request(
        ENDPOINT, headers={"cv": client_version, "v": model_version}
    )
    opener = urllib.request.build_opener(NoRedirect)
    print(f"Checking {ENDPOINT} with cv={client_version}, v={model_version}", flush=True)
    for attempt in range(1, 4):
        status = None
        try:
            with opener.open(request, timeout=15) as response:
                status = response.status
        except urllib.error.HTTPError as error:
            status = error.code
            error.close()
        except (urllib.error.URLError, TimeoutError, OSError) as error:
            detail = f"Network error: {error}"

        if status == 200:
            print(f"PASS: Tuta accepts the pinned SDK version {client_version} (HTTP 200).")
            return 0
        if status == 474:
            print(
                f"FAIL: Tuta rejects the pinned SDK version {client_version} "
                "(HTTP 474 Invalid Software Version). Update the vendored SDK; "
                "credentials cannot fix this. See issue #37.",
                file=sys.stderr,
            )
            return 1
        if status is not None:
            detail = f"Unexpected HTTP {status}"
        retryable = status is None or status in (408, 429) or 500 <= status <= 599
        if retryable and attempt < 3:
            print(f"{detail}; retrying ({attempt}/3).", flush=True)
            time.sleep(2 ** attempt)
            continue
        print(
            f"INCONCLUSIVE: {detail}. Could not verify SDK version {client_version}; "
            "this is not evidence of a version rejection.",
            file=sys.stderr,
        )
        return 2


def main():
    try:
        versions = sdk_versions()
    except (OSError, ValueError, KeyError, TypeError) as error:
        print(f"Cannot read the pinned SDK versions: {error}. Check out the submodule.", file=sys.stderr)
        return 2
    return check_version(*versions)


if __name__ == "__main__":
    sys.exit(main())
