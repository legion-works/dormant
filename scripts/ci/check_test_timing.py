#!/usr/bin/env python3
"""Reject newly added raw sleeps in test code unless their contract is audited."""

from __future__ import annotations

import argparse
import dataclasses
import pathlib
import re
import subprocess
import sys
from collections.abc import Iterable


ALLOWLIST_HEADER = (
    "path\tfunction\tanchor\towner\treason\texternal_resource\treplacement_readiness"
)
SLEEP_CALL = re.compile(
    r"(?P<call>tokio::time::sleep\s*\([^;]*\)|std::thread::sleep\s*\([^;]*\)|(?<![:\w])sleep\s*\([^;]*\))"
)
HUNK = re.compile(r"^@@ -\d+(?:,\d+)? \+(?P<line>\d+)(?:,\d+)? @@")
FUNCTION = re.compile(r"\b(?:async\s+)?fn\s+(?P<name>[A-Za-z_][A-Za-z0-9_]*)\b")
CFG_TEST = re.compile(r"#\s*\[\s*cfg\s*\(\s*test\s*\)\s*\]")


@dataclasses.dataclass(frozen=True)
class AddedLine:
    path: str
    line: int
    text: str


@dataclasses.dataclass(frozen=True)
class AllowlistRecord:
    path: str
    function: str
    anchor: str
    owner: str
    reason: str
    external_resource: str
    replacement_readiness: str


@dataclasses.dataclass(frozen=True)
class SleepFinding:
    path: str
    line: int
    function: str
    anchor: str


def parse_added_lines(diff: str) -> list[AddedLine]:
    path: str | None = None
    new_line: int | None = None
    added: list[AddedLine] = []
    for line in diff.splitlines():
        if line.startswith("diff --git "):
            path = None
            new_line = None
        elif line.startswith("+++ b/"):
            path = line[6:]
        elif match := HUNK.match(line):
            new_line = int(match.group("line"))
        elif new_line is not None and line.startswith("+") and not line.startswith("+++"):
            if path is not None:
                added.append(AddedLine(path, new_line, line[1:]))
            new_line += 1
        elif new_line is not None and line.startswith("-"):
            continue
        elif new_line is not None and not line.startswith("\\"):
            new_line += 1
    return added


def normalize_call_anchor(text: str) -> str:
    match = SLEEP_CALL.search(text)
    if match is None:
        raise ValueError(f"not a raw sleep call: {text}")
    return re.sub(r"\s+", "", match.group("call"))


def _spans(source: list[str]) -> tuple[list[tuple[int, int]], list[tuple[str, int, int]]]:
    cfg_ranges: list[tuple[int, int]] = []
    cfg_stack: list[tuple[int, int]] = []
    function_ranges: list[tuple[str, int, int]] = []
    function_stack: list[tuple[str, int, int]] = []
    cfg_pending = False
    function_pending: tuple[str, int] | None = None
    depth = 0

    for line_number, text in enumerate(source, start=1):
        if CFG_TEST.search(text):
            cfg_pending = True
        if function_pending is None:
            if match := FUNCTION.search(text):
                function_pending = (match.group("name"), line_number)

        opens = text.count("{")
        closes = text.count("}")
        if opens and cfg_pending:
            cfg_stack.append((depth, line_number))
            cfg_pending = False
        if opens and function_pending is not None:
            name, start = function_pending
            function_stack.append((name, depth, start))
            function_pending = None
        depth += opens - closes

        while cfg_stack and depth <= cfg_stack[-1][0]:
            base_depth, start = cfg_stack.pop()
            cfg_ranges.append((start, line_number))
        while function_stack and depth <= function_stack[-1][1]:
            name, base_depth, start = function_stack.pop()
            function_ranges.append((name, start, line_number))

    return cfg_ranges, function_ranges


def _is_integration_test_path(path: str) -> bool:
    return path.startswith("tests/") or "/tests/" in path


def find_sleep_calls(root: pathlib.Path, added_lines: Iterable[AddedLine]) -> list[SleepFinding]:
    findings: list[SleepFinding] = []
    by_path: dict[str, list[AddedLine]] = {}
    for added in added_lines:
        if SLEEP_CALL.search(added.text):
            by_path.setdefault(added.path, []).append(added)

    for path, candidates in by_path.items():
        if not path.endswith(".rs"):
            continue
        source_path = root / path
        try:
            source = source_path.read_text().splitlines()
        except OSError as error:
            raise RuntimeError(f"cannot inspect {path} after diff: {error}") from error
        cfg_ranges, function_ranges = _spans(source)
        for candidate in candidates:
            in_cfg_test = any(start <= candidate.line <= end for start, end in cfg_ranges)
            if not (_is_integration_test_path(path) or in_cfg_test):
                continue
            containing = [
                (name, end - start)
                for name, start, end in function_ranges
                if start <= candidate.line <= end
            ]
            function = min(containing, key=lambda item: item[1])[0] if containing else "<module>"
            findings.append(
                SleepFinding(path, candidate.line, function, normalize_call_anchor(candidate.text))
            )
    return findings


def load_allowlist(path: pathlib.Path) -> list[AllowlistRecord]:
    records: list[AllowlistRecord] = []
    for line_number, raw in enumerate(path.read_text().splitlines(), start=1):
        if not raw or raw.startswith("#") or raw == ALLOWLIST_HEADER:
            continue
        fields = raw.split("\t")
        if len(fields) != 7 or any(not field for field in fields):
            raise ValueError(
                f"{path}:{line_number}: expected seven non-empty tab-separated fields: {ALLOWLIST_HEADER}"
            )
        records.append(
            AllowlistRecord(fields[0], fields[1], re.sub(r"\s+", "", fields[2]), *fields[3:])
        )
    return records


def evaluate_findings(findings: list[SleepFinding], records: list[AllowlistRecord]) -> list[str]:
    failures: list[str] = []
    duplicate_keys: set[tuple[str, str, str]] = set()
    seen_keys: set[tuple[str, str, str]] = set()
    for finding in findings:
        key = (finding.path, finding.function, finding.anchor)
        if key in seen_keys:
            duplicate_keys.add(key)
        seen_keys.add(key)

    allowed = {(record.path, record.function, record.anchor) for record in records}
    for finding in findings:
        key = (finding.path, finding.function, finding.anchor)
        location = f"{finding.path}:{finding.line} ({finding.function})"
        if key in duplicate_keys:
            failures.append(f"{location}: duplicate anchor {finding.anchor}; split or remove raw sleeps")
        elif key not in allowed:
            failures.append(
                f"{location}: unaudited raw sleep {finding.anchor}; add one exact TSV record with header "
                f"{ALLOWLIST_HEADER}"
            )
    return failures


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--range", required=True, dest="revision_range", help="git revision range, e.g. BASE..HEAD")
    args = parser.parse_args()
    root = pathlib.Path(
        subprocess.run(
            ["git", "rev-parse", "--show-toplevel"], check=True, text=True, capture_output=True
        ).stdout.strip()
    )
    diff = subprocess.run(
        ["git", "diff", "-U0", args.revision_range],
        cwd=root,
        check=True,
        text=True,
        capture_output=True,
    ).stdout
    allowlist = load_allowlist(root / "scripts/ci/test-sleep-allowlist.tsv")
    failures = evaluate_findings(find_sleep_calls(root, parse_added_lines(diff)), allowlist)
    if failures:
        print("raw test sleep policy failed:", file=sys.stderr)
        print(*failures, sep="\n", file=sys.stderr)
        return 1
    print("raw test sleep policy: clean")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
