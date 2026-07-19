#!/usr/bin/env python3
"""Verify every checkout-bearing CI job rejects same-SHA reruns."""

from __future__ import annotations

import argparse
import pathlib
import sys

import yaml


CHECKOUT_ACTION = "actions/checkout@"
GUARD_SCRIPT = "scripts/ci/reject_required_rerun.sh"


def load_yaml(path: pathlib.Path) -> dict[str, object]:
    with path.open(encoding="utf-8") as handle:
        document = yaml.safe_load(handle) or {}
    if not isinstance(document, dict):
        raise ValueError("CI YAML root must be a mapping")
    return document


def checkout_steps(job: object) -> list[int]:
    if not isinstance(job, dict):
        return []
    steps = job.get("steps", [])
    if not isinstance(steps, list):
        return []
    return [
        index
        for index, step in enumerate(steps)
        if isinstance(step, dict)
        and isinstance(step.get("uses"), str)
        and step["uses"].startswith(CHECKOUT_ACTION)
    ]


def guard_steps(job: object) -> list[int]:
    if not isinstance(job, dict):
        return []
    steps = job.get("steps", [])
    if not isinstance(steps, list):
        return []
    return [
        index
        for index, step in enumerate(steps)
        if isinstance(step, dict)
        and isinstance(step.get("run"), str)
        and GUARD_SCRIPT in step["run"]
    ]


def validate_guards(path: pathlib.Path) -> list[str]:
    try:
        ci = load_yaml(path)
    except (OSError, ValueError, yaml.YAMLError) as error:
        return [str(error)]

    jobs = ci.get("jobs")
    if not isinstance(jobs, dict):
        return ["CI YAML jobs must be a mapping"]

    errors: list[str] = []
    for name, job in jobs.items():
        if not isinstance(name, str):
            errors.append("CI YAML job name must be a string")
            continue
        later_guards = guard_steps(job)
        for checkout_index in checkout_steps(job):
            if not any(guard_index > checkout_index for guard_index in later_guards):
                errors.append(
                    f"{name}: checkout step {checkout_index + 1} has no later rerun guard"
                )
    return errors


def guarded_job_count(path: pathlib.Path) -> int:
    ci = load_yaml(path)
    jobs = ci.get("jobs")
    if not isinstance(jobs, dict):
        raise ValueError("CI YAML jobs must be a mapping")
    return sum(bool(checkout_steps(job)) for job in jobs.values())


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("ci", type=pathlib.Path)
    args = parser.parse_args()

    errors = validate_guards(args.ci)
    if errors:
        print("rerun guard coverage check failed:", file=sys.stderr)
        for error in errors:
            print(f"- {error}", file=sys.stderr)
        return 1
    print(
        f"rerun guard coverage check passed: {guarded_job_count(args.ci)} "
        "checkout-bearing jobs guarded"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
