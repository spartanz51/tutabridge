"""Replay the complete prototype in a temporary Git index, without changing checkout."""
import argparse
import json
import os
from pathlib import Path
import subprocess
import tempfile

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("repository", type=Path)
args = parser.parse_args()
root = Path(__file__).resolve().parent.parent
manifest = json.loads((root / "manifest.json").read_text())
previous = root.parent / manifest["previous_artifact"]
previous_manifest = json.loads((previous / "manifest.json").read_text())

with tempfile.TemporaryDirectory() as temporary:
    env = dict(os.environ, GIT_INDEX_FILE=str(Path(temporary) / "index"))

    def git(*arguments):
        return subprocess.check_output(
            ["git", *arguments], cwd=args.repository, env=env
        ).decode().strip()

    git("read-tree", manifest["official_base"])
    for patch in previous_manifest["patches"]:
        git("apply", "--cached", "--whitespace=error", str(previous / patch))
    assert git("write-tree") == manifest["trees"]["baseline"]
    for patch in manifest["patches"]:
        git("apply", "--cached", "--whitespace=error", str(root / "patches" / patch))
        assert git("write-tree") == manifest["trees"][patch]
    print(json.dumps({"passed": True, "official_base": manifest["official_base"],
                      "final_tree": git("write-tree"), "patches_applied": 11}, indent=2))
