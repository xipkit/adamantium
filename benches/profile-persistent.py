#!/usr/bin/env python3
"""Collect snapshot-edit instructions using an optimized cow binary and Callgrind."""

import argparse
import csv
import pathlib
import re
import subprocess
import sys

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("binary", type=pathlib.Path)
parser.add_argument("output", type=pathlib.Path)
parser.add_argument("--valgrind", default="valgrind")
parser.add_argument("--sizes", type=int, nargs="+", default=[16, 64, 256, 1024, 4096, 16384, 65536])
parser.add_argument("--repeats", type=int, default=100)
parser.add_argument("--runs", type=int, default=3)
args = parser.parse_args()
if args.repeats <= 0 or args.runs <= 0 or any(n <= 0 or n > 65536 for n in args.sizes):
    parser.error("positive repeats/runs and sizes in 1..65536 required")
args.output.mkdir(parents=True, exist_ok=True)
with (args.output / "instructions.csv").open("w", newline="") as output:
    writer = csv.writer(output)
    writer.writerow(["collection", "backend", "size", "run", "repeats", "instructions", "instructions_per_edit"])
    for collection in ("array", "deque", "map", "set"):
        for size in args.sizes:
            for backend in ("cow", "im"):
                for run in range(1, args.runs + 1):
                    stem = args.output / f"{collection}-{backend}-{size}-{run}"
                    profile = stem.with_suffix(".callgrind")
                    result = subprocess.run([
                        args.valgrind, "--tool=callgrind", "--collect-atstart=no",
                        "--toggle-collect=*profile_snapshots*",
                        f"--callgrind-out-file={profile}", str(args.binary.resolve()),
                        "--profile", collection, backend, str(size), str(args.repeats),
                    ], capture_output=True, text=True, check=True)
                    stem.with_suffix(".log").write_text(result.stderr)
                    match = re.search(r"^summary: (\d+)$", profile.read_text(), re.MULTILINE)
                    if match is None or int(match.group(1)) == 0:
                        sys.exit(f"No instructions collected in {profile}; check profiler symbols")
                    instructions = int(match.group(1))
                    writer.writerow([collection, backend, size, run, args.repeats,
                                     instructions, instructions / args.repeats])
                    output.flush()
                    print(f"{collection} {backend} {size} run {run}: {instructions / args.repeats:.2f} Ir/edit", flush=True)
