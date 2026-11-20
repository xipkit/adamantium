#!/usr/bin/env python3
"""Exercise the opt-in CLI with real compilers; requires explicit setup in README.md."""
import argparse
import json
import os
from pathlib import Path
import shlex
import shutil
import subprocess
import tempfile

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--cli", type=Path, required=True)
parser.add_argument("--library", type=Path, required=True)
args = parser.parse_args()
cli, library = args.cli.resolve(), args.library.resolve()
fixture = Path(__file__).resolve().parent / "fixtures"
rules = {
    "blocking": "adamantium_blocking_in_async",
    "actor-blocking": "adamantium_blocking_in_actor",
    "nested": "adamantium_nested_actor_access",
    "limit": "adamantium_nested_limit",
    "race": "adamantium_race_with_timer",
}


def check(root, *extra, expected=1, lib=library, env=None, options=()):
    command = [str(cli), "check", str(root), "--format", "json"]
    command += list(options)
    if lib is not None:
        command += ["--dylint-library", str(lib)]
    command += ["--", "--locked", *extra]
    result = subprocess.run(command, capture_output=True, text=True, env=env)
    assert result.returncode == expected, (command, result.stdout, result.stderr)
    report = json.loads(result.stdout)
    assert report["schema_version"] == 3, report
    assert report["status"] == {0: "clean", 1: "findings", 2: "error"}[expected], report
    if expected != 2:
        assert not report["errors"], report
    return report


def diagnostics(report):
    return sorted((f["rule"], f["file"], f["line"]) for f in report["findings"] if f["rule"] in rules.values())


def marked(root, extra=False, member=True, targets=True):
    expected = []
    for file in ["src/lib.rs"] + (["member/src/lib.rs"] if member else []) + (["tests/selection.rs"] if targets else []):
        for number, line in enumerate((root / file).read_text().splitlines(), 1):
            marker = line.rsplit("// ", 1)[-1]
            if marker in rules or (extra and marker == "feature-blocking"):
                expected.append((rules.get(marker, rules["blocking"]), file, number))
    return sorted(expected)


# Use copies: compile-error and skip-list cases must not mutate checked-in fixtures.
with tempfile.TemporaryDirectory(prefix="adamantium-lints-") as temporary:
    root = Path(temporary) / "consumer"
    shutil.copytree(fixture, root)
    manifest = root / "Cargo.toml"
    original = manifest.read_text().replace('path = "../.."', f'path = "{fixture.parent.parent}"')
    manifest.write_text(original)
    report = check(root)
    assert diagnostics(report) == marked(root), (diagnostics(report), marked(root))
    assert diagnostics(check(root)) == diagnostics(report), "warm diagnostics differ"
    baseline = root / "baseline.json"
    baseline.write_text(json.dumps(report))
    known = check(root, expected=0, options=("--baseline", str(baseline)))
    assert not known["findings"] and known["baseline_unmatched"] == 0, known
    nested = len([x for x in marked(root) if x[0] == rules["nested"]])
    assert known["baselined"][rules["nested"]] == nested, known
    assert diagnostics(check(root, options=("--strict",))) == marked(root)
    sarif = subprocess.run([str(cli), "check", str(root), "--format", "sarif", "--dylint-library", str(library), "--", "--locked"], capture_output=True, text=True)
    assert sarif.returncode == 1, sarif.stderr
    results = json.loads(sarif.stdout)["runs"][0]["results"]
    assert sum(r["ruleId"] == rules["nested"] for r in results) == nested, results
    assert diagnostics(check(root, "--features", "extra")) == marked(root, extra=True)
    assert diagnostics(check(root, "-plint-consumer", "--lib")) == marked(root, member=False, targets=False)
    member = check(root, "--package=lint-member", "--lib")
    assert diagnostics(member) == [(rules["blocking"], "member/src/lib.rs", 2)], member
    assert diagnostics(check(root, "--package=lint-consumer", "--bin=lint-consumer")) == marked(root, member=False, targets=False), "--bin checked extra targets"
    assert not diagnostics(check(root, lib=None)), "ordinary check loaded experimental lints"

    manifest.write_text(original + '\n[workspace.metadata.adamantium]\nskip = ["member/", "tests/"]\nallow = ["adamantium_blocking_in_async", "adamantium_blocking_in_actor", "adamantium_nested_limit", "adamantium_race_with_timer"]\n[workspace.metadata.adamantium.exceptions]\nadamantium_nested_actor_access = ["src/"]\n')
    skipped = check(root)
    assert not diagnostics(skipped), skipped
    assert skipped["skipped"][rules["blocking"]] == len([x for x in marked(root) if x[0] == rules["blocking"]]), skipped
    assert skipped["skipped"][rules["actor-blocking"]] == len([x for x in marked(root) if x[0] == rules["actor-blocking"]]), skipped
    assert skipped["skipped"][rules["nested"]] == len([x for x in marked(root) if x[0] == rules["nested"]]), skipped
    assert skipped["skipped"][rules["race"]] == len([x for x in marked(root) if x[0] == rules["race"]]), skipped
    assert skipped["skipped"][rules["limit"]] == len([x for x in marked(root) if x[0] == rules["limit"]]), skipped
    manifest.write_text(original)

    missing = check(root, expected=2, lib=root / "missing.dylib")
    assert missing["errors"], missing
    corrupt = root / library.name
    corrupt.write_text("not a shared library\n")
    assert check(root, expected=2, lib=corrupt)["errors"]

    # Real findings must not mask an unavailable driver or a failed compilation.
    cargo = shutil.which("cargo")
    shim = root / "cargo-without-dylint"
    shim.write_text('#!/bin/sh\nif [ "$1" = dylint ]; then echo "dylint unavailable" >&2; exit 127; fi\nexec ' + shlex.quote(cargo) + ' "$@"\n')
    shim.chmod(0o755)
    env = dict(os.environ, CARGO=str(shim))
    failed = check(root, expected=2, env=env)
    assert "dylint unavailable" in str(failed["errors"]), failed
    with (root / "src/lib.rs").open("a") as source:
        source.write('\nfn broken() { unresolved_symbol(); }\n')
    failed = check(root, expected=2)
    assert failed["errors"], failed

print("experimental lint fixtures and CLI contract: passed")
