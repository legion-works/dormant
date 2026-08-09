#!/usr/bin/env python3
"""Check that open flake tests are represented by nightly soak filters."""

from __future__ import annotations

import argparse
import pathlib
import re
import sys
import tomllib
from typing import Any


TEST_FILTER = re.compile(r"test\(([^)]+)\)")


def load_ledger(path: pathlib.Path) -> list[dict[str, Any]]:
    with path.open("rb") as handle:
        ledger = tomllib.load(handle)
    incidents = ledger.get("incident")
    if not isinstance(incidents, list):
        raise ValueError(f"incident must be an array in {path}")
    return [incident for incident in incidents if isinstance(incident, dict)]


def soak_test_names(path: pathlib.Path) -> set[str]:
    # The filters live in shell run blocks; raw text avoids YAML changing shell expressions.
    return {match.group(1) for match in TEST_FILTER.finditer(path.read_text(encoding="utf-8"))}


def validate_test_list(incidents: list[dict[str, Any]], path: pathlib.Path) -> list[str]:
    available = path.read_text(encoding="utf-8").splitlines()
    return [
        f"{incident.get('id', 'incident')}: test no longer exists: {incident.get('test')}"
        for incident in incidents
        if incident.get("status") != "obsolete"
        if isinstance(incident.get("test"), str)
        and not any(incident["test"] in line for line in available)
    ]


def validate_parity(
    ledger_path: pathlib.Path,
    workflow_path: pathlib.Path,
    test_list_path: pathlib.Path | None = None,
) -> list[str]:
    try:
        incidents = load_ledger(ledger_path)
        filters = soak_test_names(workflow_path)
    except (OSError, tomllib.TOMLDecodeError, ValueError) as error:
        return [str(error)]

    errors: list[str] = []
    for incident in incidents:
        if incident.get("status") != "open":
            continue
        test = incident.get("test")
        short_name = test.rsplit("::", 1)[-1] if isinstance(test, str) else ""
        if not any(short_name in filter_text for filter_text in filters):
            errors.append(f"{incident.get('id', 'incident')}: open test is absent from every soak filter: {test}")

    if test_list_path is not None:
        try:
            errors.extend(validate_test_list(incidents, test_list_path))
        except OSError as error:
            errors.append(str(error))
    return errors


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--ledger", type=pathlib.Path, default=pathlib.Path(".github/flake-ledger.toml"))
    parser.add_argument("--workflow", type=pathlib.Path, default=pathlib.Path(".github/workflows/ci-soak.yml"))
    parser.add_argument("--test-list", type=pathlib.Path)
    args = parser.parse_args()

    errors = validate_parity(args.ledger, args.workflow, args.test_list)
    if errors:
        print("soak parity check failed:", file=sys.stderr)
        for error in errors:
            print(f"- {error}", file=sys.stderr)
        return 1
    print("soak parity check passed")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
