#!/usr/bin/env python3
"""Classify non-clean cargo-nextest JUnit test cases without changing their result."""

from __future__ import annotations

import argparse
import hashlib
import json
import pathlib
import re
import sys
import xml.etree.ElementTree as element_tree
from typing import Any


EXCERPT_LIMIT = 500
TIMESTAMP = re.compile(r"\b\d{4}-\d{2}-\d{2}[T ][0-2]\d:[0-5]\d:[0-5]\d(?:\.\d+)?(?:Z|[+-]\d{2}:?\d{2})?\b")
DURATION = re.compile(r"\b\d+(?:\.\d+)?(?:ns|µs|us|ms|s|m|h)\b")
HEX_ADDRESS = re.compile(r"\b0x[0-9a-fA-F]+\b")
ABSOLUTE_PATH = re.compile(r"(?<!\w)(?:[A-Za-z]:)?/[^\s:,'\"<>]+")
PID = re.compile(r"\b(?:pid|process)\s+\d+\b", re.IGNORECASE)
THREAD_ID = re.compile(r"\bthread\s+'[^']*'\s*\(\d+\)")


def normalize_signature(message: str) -> str:
    """Return a stable hash for one failure mechanism, excluding runner volatility."""
    normalized = TIMESTAMP.sub("<timestamp>", message)
    normalized = DURATION.sub("<duration>", normalized)
    normalized = HEX_ADDRESS.sub("<address>", normalized)
    normalized = ABSOLUTE_PATH.sub("<path>", normalized)
    normalized = PID.sub("<process>", normalized)
    normalized = THREAD_ID.sub("thread '<name>'", normalized)
    normalized = re.sub(r"\s+", " ", normalized).strip()
    return hashlib.sha256(normalized.encode("utf-8")).hexdigest()


def _tag_name(element: element_tree.Element) -> str:
    return element.tag.rsplit("}", maxsplit=1)[-1]


def _message(element: element_tree.Element) -> str:
    return (element.get("message") or "".join(element.itertext())).strip()


def _test_name(testcase: element_tree.Element) -> str:
    name = testcase.get("name", "<unnamed-test>")
    classname = testcase.get("classname")
    return f"{classname}::{name}" if classname else name


def _record(classification: str, test_name: str, attempt_count: int, message: str) -> dict[str, Any]:
    excerpt = re.sub(r"\s+", " ", message).strip()[:EXCERPT_LIMIT]
    return {
        "classification": classification,
        "test_name": test_name,
        "attempt_count": attempt_count,
        "signature": normalize_signature(message),
        "failure_excerpt": excerpt,
    }


def _infrastructure_record(path: pathlib.Path, message: str) -> dict[str, Any]:
    return _record("INFRA-CANDIDATE", str(path), 0, message)


def _classify_testcase(testcase: element_tree.Element) -> dict[str, Any] | None:
    children = list(testcase)
    failures = [child for child in children if _tag_name(child) == "failure"]
    flaky_failures = [child for child in children if _tag_name(child) == "flakyFailure"]
    rerun_failures = [child for child in children if _tag_name(child) == "rerunFailure"]
    errors = [child for child in children if _tag_name(child) == "error"]
    test_name = _test_name(testcase)

    flaky_wrapper = next((failure for failure in failures if failure.get("type") == "flaky failure"), None)
    if flaky_failures or flaky_wrapper is not None:
        message = _message(flaky_failures[0] if flaky_failures else flaky_wrapper)
        observed_attempt = re.search(
            r"attempt\s+(\d+)/(\d+)", _message(flaky_wrapper) if flaky_wrapper is not None else ""
        )
        attempts = int(observed_attempt.group(1)) if observed_attempt else len(flaky_failures) + 1
        return _record("FLAKE-OBSERVED", test_name, attempts, message)

    infra_failure = next(
        (
            child
            for child in [*errors, *failures]
            if any(token in f"{child.get('type', '')} {_message(child)}".lower() for token in ("timeout", "setup"))
        ),
        None,
    )
    if infra_failure is not None:
        return _record("INFRA-CANDIDATE", test_name, 1 + len(rerun_failures), _message(infra_failure))

    if failures:
        return _record("TEST-FAILURE", test_name, 1 + len(rerun_failures), _message(failures[0]))
    if errors:
        return _record("INFRA-CANDIDATE", test_name, 1 + len(rerun_failures), _message(errors[0]))
    return None


def classify_junit(path: pathlib.Path) -> list[dict[str, Any]]:
    """Return one classification record for each non-clean JUnit testcase."""
    try:
        root = element_tree.parse(path).getroot()
    except FileNotFoundError:
        return [_infrastructure_record(path, f"missing JUnit XML: {path}")]
    except element_tree.ParseError:
        return [_infrastructure_record(path, f"malformed JUnit XML: {path}")]
    except OSError as error:
        return [_infrastructure_record(path, f"unreadable JUnit XML: {error}")]

    return [
        record
        for testcase in root.iter()
        if _tag_name(testcase) == "testcase"
        if (record := _classify_testcase(testcase)) is not None
    ]


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("junit_xml", type=pathlib.Path, help="nextest JUnit XML report")
    args = parser.parse_args()

    for record in classify_junit(args.junit_xml):
        print(json.dumps(record, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
