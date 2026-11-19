#!/usr/bin/env python3
"""Contract tests for the composite GitHub Action in action.yml."""

import json
import os
from pathlib import Path
import shutil
import stat
import subprocess
import tempfile

REPO_ROOT = Path(__file__).resolve().parents[2]
ACTION_PATH = REPO_ROOT / "action.yml"
VALID_REV = "bfa0f78916cebb6c65cfaeefce45ed34339d324f"


def load_action() -> dict:
    try:
        import yaml  # type: ignore

        return yaml.safe_load(ACTION_PATH.read_text(encoding="utf-8"))
    except ModuleNotFoundError:
        result = subprocess.run(
            [
                "ruby",
                "-ryaml",
                "-rjson",
                "-e",
                "puts JSON.generate(YAML.load_file(ARGV[0]))",
                str(ACTION_PATH),
            ],
            check=True,
            capture_output=True,
            text=True,
        )
        return json.loads(result.stdout)


def step_by_name(action: dict, name: str) -> dict:
    for step in action["runs"]["steps"]:
        if step.get("name") == name:
            return step
    raise AssertionError(f"missing step {name!r} in action.yml")


def write_executable(path: Path, content: str) -> None:
    path.write_text(content, encoding="utf-8")
    path.chmod(path.stat().st_mode | stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH)


def run_bash(script: str, env: dict[str, str], cwd: Path) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        ["bash", "--noprofile", "--norc", "-eo", "pipefail", "-c", script],
        env=env,
        cwd=cwd,
        capture_output=True,
        text=True,
    )


def parse_github_output(path: Path) -> dict[str, str]:
    outputs: dict[str, str] = {}
    for line in path.read_text(encoding="utf-8").splitlines():
        if "=" in line:
            key, value = line.split("=", 1)
            outputs[key] = value
    return outputs


def test_metadata(action: dict) -> None:
    inputs = action["inputs"]
    for name in ("version", "rev", "working-directory", "baseline", "strict", "cargo-args"):
        assert name in inputs, f"missing input {name!r}"
    assert inputs["version"]["required"] is False
    assert inputs["rev"]["required"] is False
    assert inputs["working-directory"]["default"] == "."
    assert inputs["strict"]["default"] == "false"

    cache_step = step_by_name(action, "Restore cargo-adamantium")
    assert cache_step["with"]["key"] == "${{ steps.cache-key.outputs.key }}"
    assert "${{ steps.cache-key.outputs.key }}" in cache_step["with"]["path"]


def test_cache_key_and_validation(action: dict, tmp: Path) -> None:
    script = step_by_name(action, "Identify binary cache")["run"]
    stub_bin = tmp / "stub-rustc-bin"
    stub_bin.mkdir()
    write_executable(
        stub_bin / "rustc",
        '#!/usr/bin/env bash\nprintf "%s\\n" "${FAKE_RUSTC_VV:-rustc 1.96.0 (fake)}"\n',
    )

    def run_cache_step(
        *,
        version: str = "",
        rev: str = "",
        strict: str = "false",
        runner_os: str = "Linux",
        runner_arch: str = "X64",
        rustc_vv: str = "rustc 1.96.0 (fake)",
    ) -> tuple[subprocess.CompletedProcess[str], dict[str, str]]:
        out_file = tmp / "github_output"
        out_file.write_text("", encoding="utf-8")
        env = {
            "PATH": f"{stub_bin}:{os.environ['PATH']}",
            "GITHUB_OUTPUT": str(out_file),
            "RUNNER_OS": runner_os,
            "RUNNER_ARCH": runner_arch,
            "ADAMANTIUM_VERSION": version,
            "ADAMANTIUM_REV": rev,
            "ADAMANTIUM_STRICT": strict,
            "FAKE_RUSTC_VV": rustc_vv,
        }
        proc = run_bash(script, env, tmp)
        return proc, parse_github_output(out_file)

    # Exact crates.io release
    proc, out = run_cache_step(version="0.2.0")
    assert proc.returncode == 0, proc.stderr
    key_v020 = out["key"]
    assert key_v020.startswith("adamantium-v1-Linux-X64-")
    assert key_v020.endswith("-crates-io-0.2.0"), key_v020

    # Prerelease version
    proc, out = run_cache_step(version="0.3.0-rc.1")
    assert proc.returncode == 0, proc.stderr
    assert out["key"].endswith("-crates-io-0.3.0-rc.1"), out["key"]

    # Existing rev-only usage
    proc, out = run_cache_step(rev=VALID_REV, strict="")
    assert proc.returncode == 0, proc.stderr
    key_rev = out["key"]
    assert key_rev.startswith("adamantium-v1-Linux-X64-")
    assert key_rev.endswith(f"-git-{VALID_REV}"), key_rev
    assert key_rev != key_v020

    # Uppercase SHA normalizes to the same cache key
    proc, out = run_cache_step(rev=VALID_REV.upper())
    assert proc.returncode == 0, proc.stderr
    assert out["key"] == key_rev

    # Compiler and platform changes alter the cache key
    proc, out = run_cache_step(version="0.2.0", rustc_vv="rustc 1.97.0 (other)")
    assert proc.returncode == 0, proc.stderr
    assert out["key"] != key_v020

    proc, out = run_cache_step(version="0.2.0", runner_os="macOS", runner_arch="ARM64")
    assert proc.returncode == 0, proc.stderr
    assert out["key"].startswith("adamantium-v1-macOS-ARM64-")
    assert out["key"] != key_v020

    # Mutually exclusive / missing source
    proc, _ = run_cache_step(version="0.2.0", rev=VALID_REV)
    assert proc.returncode != 0
    assert "mutually exclusive" in proc.stderr

    proc, _ = run_cache_step()
    assert proc.returncode != 0
    assert "version" in proc.stderr and "rev" in proc.stderr

    # Invalid versions
    for bad_version in (
        "v0.2.0",
        "^0.2.0",
        "~0.2.0",
        "=0.2.0",
        ">=0.2.0",
        "0.2",
        "0.2.*",
        "*",
        "01.2.3",
        "0.02.3",
        "0.2.0-01",
        "0.2.0-",
        "0.2.0+build.1",
        "0.2.0, <1.0.0",
        " 0.2.0",
        "0.2.0 ",
        "0.2.0\n",
    ):
        proc, _ = run_cache_step(version=bad_version)
        assert proc.returncode != 0, f"expected failure for version {bad_version!r}"

    # Invalid revs
    for bad_rev in (
        "main",
        "bfa0f78",
        "a" * 39,
        "a" * 41,
        "g" * 40,
        f"{VALID_REV}\n",
    ):
        proc, _ = run_cache_step(rev=bad_rev)
        assert proc.returncode != 0, f"expected failure for rev {bad_rev!r}"

    # Invalid strict values
    for bad_strict in ("yes", "1", "TRUE", "False"):
        proc, _ = run_cache_step(version="0.2.0", strict=bad_strict)
        assert proc.returncode != 0, f"expected failure for strict {bad_strict!r}"

    proc, _ = run_cache_step(version="0.2.0", strict="true")
    assert proc.returncode == 0, proc.stderr


def make_cargo_stub(bin_dir: Path) -> None:
    write_executable(
        bin_dir / "cargo",
        """#!/usr/bin/env python3
import json, os, sys
record = {"argv": sys.argv[1:], "path": os.environ.get("PATH", "")}
with open(os.environ["CARGO_LOG"], "w", encoding="utf-8") as fh:
    json.dump(record, fh)
""",
    )


def test_install_step(action: dict, tmp: Path) -> None:
    script = step_by_name(action, "Install cargo-adamantium")["run"]
    stub_bin = tmp / "stub-install-bin"
    stub_bin.mkdir()
    make_cargo_stub(stub_bin)

    runner_temp = tmp / "runner-temp-install"
    runner_temp.mkdir()
    log_file = tmp / "install-cargo.json"

    def run_install(*, version: str = "", rev: str = "", cache_key: str) -> dict | None:
        if log_file.exists():
            log_file.unlink()
        env = {
            "PATH": f"{stub_bin}:{os.environ['PATH']}",
            "RUNNER_TEMP": str(runner_temp),
            "CARGO_LOG": str(log_file),
            "ADAMANTIUM_VERSION": version,
            "ADAMANTIUM_REV": rev,
            "ADAMANTIUM_CACHE_KEY": cache_key,
        }
        proc = run_bash(script, env, tmp)
        assert proc.returncode == 0, proc.stderr
        if not log_file.exists():
            return None
        return json.loads(log_file.read_text(encoding="utf-8"))

    key_ver = "adamantium-v1-Linux-X64-deadbeef-crates-io-0.2.0"
    rec = run_install(version="0.2.0", cache_key=key_ver)
    assert rec is not None
    assert rec["argv"] == [
        "install",
        "--locked",
        "--registry",
        "crates-io",
        "--version",
        "=0.2.0",
        "--root",
        str(runner_temp / "adamantium" / key_ver),
        "cargo-adamantium",
    ]

    key_rev = f"adamantium-v1-Linux-X64-deadbeef-git-{VALID_REV}"
    rec = run_install(rev=VALID_REV.upper(), cache_key=key_rev)
    assert rec is not None
    assert rec["argv"] == [
        "install",
        "--locked",
        "--git",
        "https://github.com/xipkit/adamantium",
        "--rev",
        VALID_REV,
        "--root",
        str(runner_temp / "adamantium" / key_rev),
        "cargo-adamantium",
    ]

    # Existing binary under the same cache key skips redundant cargo install
    existing_bin = runner_temp / "adamantium" / key_ver / "bin" / "cargo-adamantium"
    existing_bin.parent.mkdir(parents=True, exist_ok=True)
    write_executable(existing_bin, "#!/usr/bin/env bash\nexit 0\n")
    rec = run_install(version="0.2.0", cache_key=key_ver)
    assert rec is None, f"expected install to be skipped, got {rec}"


def test_run_step(action: dict, tmp: Path) -> None:
    script = step_by_name(action, "Run adamantium check")["run"]
    stub_bin = tmp / "stub-run-bin"
    stub_bin.mkdir()
    make_cargo_stub(stub_bin)

    workdir = tmp / "workdir"
    workdir.mkdir()
    # Create files so accidental globbing of `*` would visibly expand them.
    (workdir / "alpha.rs").write_text("", encoding="utf-8")
    (workdir / "beta.rs").write_text("", encoding="utf-8")

    runner_temp = tmp / "runner-temp-run"
    runner_temp.mkdir()
    cache_key = "adamantium-v1-Linux-X64-deadbeef-crates-io-0.2.0"
    log_file = tmp / "run-cargo.json"

    def run_check(
        *,
        baseline: str = "",
        strict: str = "false",
        cargo_args: str = "",
    ) -> tuple[subprocess.CompletedProcess[str], dict | None]:
        if log_file.exists():
            log_file.unlink()
        env = {
            "PATH": f"{stub_bin}:{os.environ['PATH']}",
            "RUNNER_TEMP": str(runner_temp),
            "CARGO_LOG": str(log_file),
            "ADAMANTIUM_BASELINE": baseline,
            "ADAMANTIUM_STRICT": strict,
            "ADAMANTIUM_CARGO_ARGS": cargo_args,
            "ADAMANTIUM_CACHE_KEY": cache_key,
        }
        proc = run_bash(script, env, workdir)
        rec = json.loads(log_file.read_text(encoding="utf-8")) if log_file.exists() else None
        return proc, rec

    # Existing rev-only / default options
    proc, rec = run_check()
    assert proc.returncode == 0, proc.stderr
    assert rec is not None
    assert rec["argv"] == ["adamantium", "check"]
    assert rec["path"].startswith(f"{runner_temp / 'adamantium' / cache_key / 'bin'}:")

    # Whitespace-only cargo-args does not append a stray `--`
    proc, rec = run_check(cargo_args="  \t \n ")
    assert proc.returncode == 0, proc.stderr
    assert rec is not None
    assert rec["argv"] == ["adamantium", "check"]

    # Strict mode
    proc, rec = run_check(strict="true")
    assert proc.returncode == 0, proc.stderr
    assert rec is not None
    assert rec["argv"] == ["adamantium", "check", "--strict"]

    proc, _ = run_check(strict="yes")
    assert proc.returncode != 0

    # Baseline with spaces and leading hyphen
    proc, rec = run_check(baseline="my baselines/report 1.json")
    assert proc.returncode == 0, proc.stderr
    assert rec is not None
    assert rec["argv"] == ["adamantium", "check", "--baseline=my baselines/report 1.json"]

    proc, rec = run_check(baseline="-report.json")
    assert proc.returncode == 0, proc.stderr
    assert rec is not None
    assert rec["argv"] == ["adamantium", "check", "--baseline=-report.json"]

    # All-targets locked check
    proc, rec = run_check(cargo_args="--all-targets --locked")
    assert proc.returncode == 0, proc.stderr
    assert rec is not None
    assert rec["argv"] == ["adamantium", "check", "--", "--all-targets", "--locked"]

    # Quoted spaces, adjacent quotes, empty arg, escapes, multiline, CRLF
    proc, rec = run_check(
        cargo_args=(
            '--features "tokio serde" '
            "--manifest-path 'my crate/Cargo.toml' "
            '--manifest-path="other dir/Cargo.toml" '
            '--empty "" '
            '--cfg "feature=\\"x\\"" '
            '--win="a\\\\b" '
            '--keep="a\\zb" '
            "--raw='a\\nb' "
            '--joined "ab\\\ncd" '
            '--joined-crlf "ef\\\r\ngh" '
            '--wordjoin xy\\\nzw '
            "--cont \\\n--locked\r\n"
            "--spaced foo\\ bar "
            "--backslash \\\\\r\n"
        )
    )
    assert proc.returncode == 0, proc.stderr
    assert rec is not None
    assert rec["argv"] == [
        "adamantium",
        "check",
        "--",
        "--features",
        "tokio serde",
        "--manifest-path",
        "my crate/Cargo.toml",
        "--manifest-path=other dir/Cargo.toml",
        "--empty",
        "",
        "--cfg",
        'feature="x"',
        "--win=a\\b",
        "--keep=a\\zb",
        "--raw=a\\nb",
        "--joined",
        "abcd",
        "--joined-crlf",
        "efgh",
        "--wordjoin",
        "xyzw",
        "--cont",
        "--locked",
        "--spaced",
        "foo bar",
        "--backslash",
        "\\",
    ]

    # Globbing and command substitution must never occur
    proc, rec = run_check(
        cargo_args='--exclude * --features "$(touch pwned)" --config \'a;touch pwned2\''
    )
    assert proc.returncode == 0, proc.stderr
    assert rec is not None
    assert rec["argv"] == [
        "adamantium",
        "check",
        "--",
        "--exclude",
        "*",
        "--features",
        "$(touch pwned)",
        "--config",
        "a;touch pwned2",
    ]
    assert not (workdir / "pwned").exists()
    assert not (workdir / "pwned2").exists()

    # Combined options
    proc, rec = run_check(
        strict="true",
        baseline="my baselines/strict.json",
        cargo_args='--all-targets --locked --features "a b"',
    )
    assert proc.returncode == 0, proc.stderr
    assert rec is not None
    assert rec["argv"] == [
        "adamantium",
        "check",
        "--strict",
        "--baseline=my baselines/strict.json",
        "--",
        "--all-targets",
        "--locked",
        "--features",
        "a b",
    ]

    # Unterminated quotes / trailing escape fail cleanly
    for bad_args in (
        '--features "unclosed',
        "--features 'unclosed",
        "--locked \\",
        '--features "trailing\\',
    ):
        proc, _ = run_check(cargo_args=bad_args)
        assert proc.returncode != 0, f"expected failure for cargo-args {bad_args!r}"
        assert "unterminated" in proc.stderr


def main() -> None:
    action = load_action()
    test_metadata(action)
    tmp = Path(tempfile.mkdtemp(prefix="adamantium-action-test-"))
    try:
        test_cache_key_and_validation(action, tmp)
        test_install_step(action, tmp)
        test_run_step(action, tmp)
    finally:
        shutil.rmtree(tmp, ignore_errors=True)
    print("action.yml contract tests passed")


if __name__ == "__main__":
    main()
