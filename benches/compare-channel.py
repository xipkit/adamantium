#!/usr/bin/env python3
"""Alternate two channel benchmark binaries and interleave a same-binary control."""

import argparse
import json
import os
import statistics
import subprocess

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("baseline")
parser.add_argument("candidate")
parser.add_argument("--pairs", type=int, default=20)
parser.add_argument("--capacity", type=int, default=64)
parser.add_argument("--count", type=int, default=1_000_000)
parser.add_argument("--producers", type=int, default=4)
parser.add_argument("--receivers", type=int, default=4)
args = parser.parse_args()


def run(binary):
    return int(subprocess.check_output(
        [binary, str(args.capacity), str(args.count), str(args.producers), str(args.receivers)], text=True, timeout=120
    )) / 1_000_000


for _ in range(3):
    run(args.baseline)
    run(args.candidate)

records = []
for pair in range(args.pairs):
    for kind in (["AB", "AA"] if pair % 2 == 0 else ["AA", "AB"]):
        binaries = {"A": args.baseline,
                    "B": args.candidate if kind == "AB" else args.baseline}
        order = ["A", "B"] if pair % 2 == 0 else ["B", "A"]
        times = {arm: run(binaries[arm]) for arm in order}
        row = {"kind": kind, "pair": pair, "order": order,
               "milliseconds": times, "load": os.getloadavg()}
        records.append(row)
        print(json.dumps(row), flush=True)

for kind in ["AB", "AA"]:
    rows = [r["milliseconds"] for r in records if r["kind"] == kind]
    ratios = [r["B"] / r["A"] for r in rows]
    print(json.dumps({"summary": kind, "capacity": args.capacity,
                      "producers": args.producers, "receivers": args.receivers,
                      "messages": args.producers * args.count, "pairs": args.pairs,
                      "median_a_ms": statistics.median(r["A"] for r in rows),
                      "median_b_ms": statistics.median(r["B"] for r in rows),
                      "median_paired_ratio": statistics.median(ratios),
                      "paired_ratio_range": [min(ratios), max(ratios)],
                      "paired_ratio_quartiles": statistics.quantiles(ratios, n=4)}),
          flush=True)
