#!/usr/bin/env python3
"""Verify every checkout-bearing CI job rejects same-SHA reruns."""

from __future__ import annotations

import argparse
import pathlib
import re
import sys

import yaml


CHECKOUT_ACTION = "actions/checkout@"
GUARD_SCRIPT = "scripts/ci/reject_required_rerun.sh"
GUARD_INVOCATION = re.compile(
    rf"(?:bash|sh)\s+(?:\./)?{re.escape(GUARD_SCRIPT)}|(?:\./)?{re.escape(GUARD_SCRIPT)}"
)


def load_yaml(path: pathlib.Path) -> dict[str, object]:
    with path.open(encoding="utf-8") as handle:
        document = yaml.safe_load(handle) or {}
    if not isinstance(document, dict):
        raise ValueError("CI YAML root must be a mapping")
    return document


def job_steps(job: object) -> list[object]:
    if not isinstance(job, dict):
        return []
    steps = job.get("steps", [])
    if not isinstance(steps, list):
        return []
    return steps


def checkout_steps(job: object) -> list[int]:
    return [
        index
        for index, step in enumerate(job_steps(job))
        if isinstance(step, dict)
        and isinstance(step.get("uses"), str)
        and step["uses"].startswith(CHECKOUT_ACTION)
    ]


def guard_steps(job: object) -> list[int]:
    return [
        index
        for index, step in enumerate(job_steps(job))
        if isinstance(step, dict)
        and isinstance(step.get("run"), str)
        and GUARD_INVOCATION.fullmatch(step["run"].strip()) is not None
        and "if" not in step
        and not step.get("continue-on-error", False)
    ]


def uses_run_attempt_condition(value: object) -> bool:
    return isinstance(value, str) and "github.run_attempt" in value.lower()


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
        if isinstance(job, dict) and uses_run_attempt_condition(job.get("if")):
            errors.append(f"{name}: job if condition references github.run_attempt")
        for step_index, step in enumerate(job_steps(job)):
            if isinstance(step, dict) and uses_run_attempt_condition(step.get("if")):
                errors.append(
                    f"{name}: step {step_index + 1} if condition references github.run_attempt"
                )
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
