#!/usr/bin/env python3
"""Stage the exact native library built on this host and its CLI release pin."""
import argparse
import hashlib
import json
from pathlib import Path
import shutil
import subprocess

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("platform", choices=["linux-x86_64", "darwin-arm64"])
parser.add_argument("build", type=Path, help="Cargo target directory")
parser.add_argument("output", type=Path)
args = parser.parse_args()

host, suffix = {
    "linux-x86_64": ("x86_64-unknown-linux-gnu", "so"),
    "darwin-arm64": ("aarch64-apple-darwin", "dylib"),
}[args.platform]
metadata = json.loads(subprocess.check_output(
    ["cargo", "metadata", "--locked", "--no-deps", "--format-version", "1"]
))
version = next(p["version"] for p in metadata["packages"] if p["name"] == "cargo-adamantium")
# The linker records the exact rustup toolchain in Dylint's output filename.
library, = (args.build / "release").glob(f"libadamantium_lints@nightly-*-{host}.{suffix}")
nightly = library.name.split("@", 1)[1].removesuffix(f"-{host}.{suffix}")
rustc = dict(line.split(": ", 1) for line in subprocess.check_output(
    ["rustup", "run", f"{nightly}-{host}", "rustc", "-vV"], text=True
).splitlines() if ": " in line)
if rustc["host"] != host:
    raise SystemExit(f"expected native compiler for {host}")
args.output.mkdir(parents=True, exist_ok=True)
asset = args.output / f"adamantium-lints-{args.platform}.{suffix}"
shutil.copyfile(library, asset)
pin = dict(version=version, host=host, nightly=nightly, rustc_commit=rustc["commit-hash"],
           sha256=hashlib.sha256(asset.read_bytes()).hexdigest(), size=asset.stat().st_size)
(args.output / f"{args.platform}.json").write_text(json.dumps(pin, indent=2) + "\n")
