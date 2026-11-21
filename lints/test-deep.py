#!/usr/bin/env python3
"""Run the release-pinned CLI against a fixture and then damage the cached copy."""
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile

cli, assets = map(Path, sys.argv[1:3])
platform = sys.argv[3]
pin = json.loads((assets / f"{platform}.json").read_text())
asset, = assets.glob(f"adamantium-lints-{platform}.*")
with tempfile.TemporaryDirectory() as scratch:
    home = Path(scratch)
    real_home = Path(os.environ.get("CARGO_HOME", Path.home() / ".cargo"))
    for name in ["registry", "git"]:
        if (real_home / name).is_dir():
            (home / name).symlink_to((real_home / name).resolve(), target_is_directory=True)
    cache = home / "adamantium" / pin["version"] / asset.name
    cache.parent.mkdir(parents=True)
    shutil.copyfile(asset, cache)
    command = [str(cli.resolve()), "check", "lints/fixtures", "--deep", "--format", "json",
               "--", "--locked", "-p", "lint-member", "--lib"]
    env = dict(os.environ, CARGO_HOME=str(home))
    def check(expected):
        result = subprocess.run(command, env=env, capture_output=True, text=True)
        assert result.returncode == expected, (result.stdout, result.stderr)
        return json.loads(result.stdout)
    report = check(1)
    assert any(f["rule"] == "adamantium_blocking_in_async" for f in report["findings"]), report
    with cache.open("r+b") as output:
        first = output.read(1)
        output.seek(0)
        output.write(bytes([first[0] ^ 1]))
    report = check(2)
    assert report["status"] == "error" and "SHA-256" in str(report["errors"]), report
print("cached deep pass and checksum failure: passed")
