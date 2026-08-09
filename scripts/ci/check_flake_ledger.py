#!/usr/bin/env python3
"""Validate the durable CI flake incident ledger."""

from __future__ import annotations

import argparse
import datetime
import pathlib
import re
import sys
import tomllib
from typing import Any


REQUIRED_KEYS = frozenset(
    {
        "id",
        "test",
        "first_seen",
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
GITHUB_RUN_URL = re.compile(r"https://github\.com/[^/]+/[^/]+/actions/runs/[1-9][0-9]*\/?\Z")
ISO_DATE = re.compile(r"[0-9]{4}-[0-9]{2}-[0-9]{2}\Z")


def _incident_label(incident: dict[str, Any], index: int) -> str:
    incident_id = incident.get("id")
    return incident_id if isinstance(incident_id, str) and incident_id else f"incident #{index}"


def _is_nonempty_string(value: Any) -> bool:
    return isinstance(value, str) and bool(value.strip())


def _has_valid_url(value: Any) -> bool:
    return isinstance(value, str) and bool(GITHUB_RUN_OR_PR_URL.fullmatch(value))


def _has_valid_run_url(value: Any) -> bool:
    return isinstance(value, str) and bool(GITHUB_RUN_URL.fullmatch(value))


def _parse_date(value: Any) -> datetime.date | None:
    if not isinstance(value, str) or not ISO_DATE.fullmatch(value):
        return None
    try:
        return datetime.date.fromisoformat(value)
    except ValueError:
        return None


def validate_ledger(path: pathlib.Path, *, today: datetime.date | None = None) -> list[str]:
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

    today = today or datetime.date.today()
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

        first_seen = _parse_date(incident["first_seen"])
        if first_seen is None:
            errors.append(f"{label}: first_seen must be an ISO YYYY-MM-DD date")

        if not isinstance(incident["signature"], str) or not SIGNATURE.fullmatch(incident["signature"]):
            errors.append(
                f"{label}: signature must be sha256: followed by 64 hex characters "
                "(prefix bare classifier output with sha256:)"
            )

        for field in ("mechanism", "reproduction"):
            if not _is_nonempty_string(incident[field]):
                errors.append(f"{label}: {field} must be a non-empty string")

        status = incident["status"]
        if status not in {"open", "fixed", "obsolete"}:
            errors.append(f"{label}: status must be 'open', 'fixed', or 'obsolete'")
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
                # The moment a field accepts two kinds of evidence, the cheaper one becomes the default.
                if not _has_valid_run_url(evidence_url):
                    errors.append(f"{label}: soak_evidence[{evidence_index}] must be an HTTPS github.com Actions run URL")

        deferred_until = incident.get("triage_deferred_until")
        deferred_reason = incident.get("triage_deferred_reason")
        if (deferred_until is None) != (deferred_reason is None):
            errors.append(f"{label}: triage deferral requires both triage_deferred_until and triage_deferred_reason")
        deferral_date = None
        if deferred_until is not None and deferred_reason is not None:
            deferral_date = _parse_date(deferred_until)
            if deferral_date is None:
                errors.append(f"{label}: triage_deferred_until must be an ISO YYYY-MM-DD date")
            if not _is_nonempty_string(deferred_reason):
                errors.append(f"{label}: triage_deferred_reason must be a non-empty string")
            if deferral_date is not None and first_seen is not None:
                cap = max(first_seen, today) + datetime.timedelta(days=90)
                if deferral_date > cap:
                    errors.append(f"{label}: triage deferral cannot exceed 90 days from the later of first_seen or today")
                if deferral_date < today:
                    errors.append(f"{label}: triage_deferred_until is in the past")

        if status == "obsolete":
            if not _is_nonempty_string(incident["root_cause"]):
                errors.append(f"{label}: obsolete incidents require non-empty root_cause")
            continue

        if status == "open" and first_seen is not None and today - first_seen > datetime.timedelta(days=30):
            if not _is_nonempty_string(incident["root_cause"]) and deferral_date is None:
                errors.append(f"{label}: open incidents older than 30 days require root_cause or an active triage deferral")

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
    parser.add_argument("--today", type=datetime.date.fromisoformat, default=datetime.date.today())
    args = parser.parse_args()

    errors = validate_ledger(args.ledger, today=args.today)
    if errors:
        print("flake ledger validation failed:", file=sys.stderr)
        for error in errors:
            print(f"- {error}", file=sys.stderr)
        return 1
    print(f"flake ledger valid: {args.ledger}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
