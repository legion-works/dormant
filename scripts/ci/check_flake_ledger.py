#!/usr/bin/env python3
"""Validate the durable CI flake incident ledger."""

from __future__ import annotations

import argparse
import pathlib
import re
import sys
import tomllib
from typing import Any


REQUIRED_KEYS = frozenset(
    {
        "id",
        "test",
        "signature",
        "first_run_url",
        "last_run_url",
        "mechanism",
        "reproduction",
        "root_cause",
        "fix_pr",
        "proving_test",
        "soak_evidence",
        "status",
    }
)
EVIDENCE_FIELDS = (
    "first_run_url",
    "last_run_url",
    "root_cause",
    "fix_pr",
    "proving_test",
)
SIGNATURE = re.compile(r"sha256:[0-9a-fA-F]{64}\Z")
GITHUB_RUN_OR_PR_URL = re.compile(
    r"https://github\.com/[^/]+/[^/]+/(?:actions/runs/[1-9][0-9]*|pull/[1-9][0-9]*)/?\Z"
)


def _incident_label(incident: dict[str, Any], index: int) -> str:
    incident_id = incident.get("id")
    return incident_id if isinstance(incident_id, str) and incident_id else f"incident #{index}"


def _is_nonempty_string(value: Any) -> bool:
    return isinstance(value, str) and bool(value.strip())


def _has_valid_url(value: Any) -> bool:
    return isinstance(value, str) and bool(GITHUB_RUN_OR_PR_URL.fullmatch(value))


def validate_ledger(path: pathlib.Path) -> list[str]:
    """Return actionable validation errors for the TOML ledger at ``path``."""
    try:
        with path.open("rb") as ledger_file:
            ledger = tomllib.load(ledger_file)
    except FileNotFoundError:
        return [f"ledger file not found: {path}"]
    except tomllib.TOMLDecodeError as error:
        return [f"invalid TOML in {path}: {error}"]
    except OSError as error:
        return [f"unable to read ledger {path}: {error}"]

    errors: list[str] = []
    if ledger.get("schema_version") != 1:
        errors.append("schema_version must be present and equal to 1")

    incidents = ledger.get("incident")
    if not isinstance(incidents, list) or not incidents:
        return [*errors, "incident must be a non-empty array of tables"]

    ids: set[str] = set()
    tests: set[str] = set()
    for index, incident in enumerate(incidents, start=1):
        if not isinstance(incident, dict):
            errors.append(f"incident #{index}: must be a TOML table")
            continue

        label = _incident_label(incident, index)
        missing = REQUIRED_KEYS.difference(incident)
        for field in sorted(missing):
            errors.append(f"{label}: missing required field '{field}'")
        if missing:
            continue

        incident_id = incident["id"]
        if not _is_nonempty_string(incident_id):
            errors.append(f"{label}: id must be a non-empty string")
        elif incident_id in ids:
            errors.append(f"{label}: duplicate id '{incident_id}'")
        else:
            ids.add(incident_id)

        test_name = incident["test"]
        if not _is_nonempty_string(test_name):
            errors.append(f"{label}: test must be a non-empty string")
        elif test_name in tests:
            errors.append(f"{label}: duplicate test '{test_name}'")
        else:
            tests.add(test_name)

        if not isinstance(incident["signature"], str) or not SIGNATURE.fullmatch(incident["signature"]):
            errors.append(f"{label}: signature must be sha256: followed by 64 hex characters")

        for field in ("mechanism", "reproduction"):
            if not _is_nonempty_string(incident[field]):
                errors.append(f"{label}: {field} must be a non-empty string")

        status = incident["status"]
        if status not in {"open", "fixed"}:
            errors.append(f"{label}: status must be 'open' or 'fixed'")
            continue

        for field in ("first_run_url", "last_run_url", "fix_pr"):
            value = incident[field]
            if value and not _has_valid_url(value):
                errors.append(f"{label}: {field} must be an HTTPS github.com run or PR URL")

        soak_evidence = incident["soak_evidence"]
        if not isinstance(soak_evidence, list):
            errors.append(f"{label}: soak_evidence must be an array of HTTPS github.com run or PR URLs")
        else:
            for evidence_index, evidence_url in enumerate(soak_evidence, start=1):
                if not _has_valid_url(evidence_url):
                    errors.append(
                        f"{label}: soak_evidence[{evidence_index}] must be an HTTPS github.com run or PR URL"
                    )

        if status == "fixed":
            for field in EVIDENCE_FIELDS:
                if not _is_nonempty_string(incident[field]):
                    errors.append(f"{label}: fixed incidents require non-empty {field}")
            if not isinstance(soak_evidence, list) or not soak_evidence:
                errors.append(f"{label}: fixed incidents require at least one soak_evidence URL")

    return errors


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("ledger", type=pathlib.Path, help="flake-ledger TOML path")
    args = parser.parse_args()

    errors = validate_ledger(args.ledger)
    if errors:
        print("flake ledger validation failed:", file=sys.stderr)
        for error in errors:
            print(f"- {error}", file=sys.stderr)
        return 1
    print(f"flake ledger valid: {args.ledger}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
