#!/usr/bin/env python3
"""Prove that the CI nextest profile observes retries without laundering flakes."""

from __future__ import annotations

import argparse
import os
import pathlib
import shutil
import subprocess
import sys
import tempfile

import classify_nextest_junit


ROOT = pathlib.Path(__file__).resolve().parents[2]
FIXTURE = ROOT / ".github/fixtures/nextest-policy"
CONFIG = ROOT / ".config/nextest.toml"
# Nextest resolves JUnit paths below the selected profile report directory. The
# standalone fixture owns its target directory, so its report stays outside the workspace.
JUNIT = FIXTURE / "target/nextest/ci/junit.xml"


class PolicyFailure(RuntimeError):
    """The fixture did not prove the required nextest behavior."""


def run_case(test_name: str, state_dir: pathlib.Path) -> tuple[int, list[dict[str, object]]]:
    if JUNIT.exists():
        JUNIT.unlink()
    environment = os.environ | {"NEXTEST_POLICY_STATE_DIR": str(state_dir)}
    completed = subprocess.run(
        [
            "cargo",
            "nextest",
            "run",
            "--manifest-path",
            str(FIXTURE / "Cargo.toml"),
            "--config-file",
            str(CONFIG),
            "--profile",
            "ci",
            "-E",
            f"test({test_name})",
        ],
        cwd=ROOT,
        env=environment,
        text=True,
        capture_output=True,
    )
    if not JUNIT.is_file():
        raise PolicyFailure(
            f"{test_name}: nextest did not produce JUnit at {JUNIT}; stderr:\n{completed.stderr}"
        )
    return completed.returncode, classify_nextest_junit.classify_junit(JUNIT)


def require_case(
    test_name: str,
    state_dir: pathlib.Path,
    expected_exit_nonzero: bool,
    expected_classification: str | None,
) -> None:
    returncode, records = run_case(test_name, state_dir)
    if (returncode != 0) != expected_exit_nonzero:
        expectation = "non-zero" if expected_exit_nonzero else "zero"
        raise PolicyFailure(f"{test_name}: expected {expectation} exit, got {returncode}")
    if expected_classification is None:
        if records:
            raise PolicyFailure(f"{test_name}: expected a clean JUnit report, got {records}")
        print(f"{test_name}: exit={returncode} clean")
        return
    classifications = [record["classification"] for record in records]
    if expected_classification not in classifications:
        raise PolicyFailure(
            f"{test_name}: expected {expected_classification}, got {classifications or 'no classifications'}"
        )
    print(f"{test_name}: exit={returncode} classification={expected_classification}")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--nextest-version", required=True, help="required cargo-nextest version")
    args = parser.parse_args()

    installed_version = subprocess.run(
        ["cargo", "nextest", "--version"],
        cwd=ROOT,
        text=True,
        capture_output=True,
        check=False,
    )
    if installed_version.returncode != 0 or args.nextest_version not in installed_version.stdout:
        print(
            f"required cargo-nextest {args.nextest_version}; got {installed_version.stdout.strip() or installed_version.stderr.strip()}",
            file=sys.stderr,
        )
        return 1

    shutil.rmtree(FIXTURE / "target/nextest/ci", ignore_errors=True)
    with tempfile.TemporaryDirectory(prefix="nextest-policy-") as directory:
        state_dir = pathlib.Path(directory)
        try:
            require_case("fail_once", state_dir, True, "FLAKE-OBSERVED")
            require_case("always_fails", state_dir, True, "TEST-FAILURE")
            require_case("always_passes", state_dir, False, None)
        except PolicyFailure as error:
            print(f"nextest policy verification failed: {error}", file=sys.stderr)
            return 1
    print("nextest policy verification: passed")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
