#!/usr/bin/env python3
"""Create or update deduplicated GitHub issues for classified soak failures."""

from __future__ import annotations

import argparse
import hashlib
import json
import pathlib
import re
import subprocess
import sys
import tomllib
from dataclasses import dataclass
from typing import Any, Protocol

from classify_nextest_junit import normalize_signature


EXCERPT_LIMIT = 4 * 1024
CONTROL_CHARACTERS = re.compile(r"[\x00-\x08\x0b-\x1f\x7f-\x9f]")


@dataclass(frozen=True)
class RunContext:
    """Metadata that identifies the soak run where a failure was observed."""

    run_url: str
    os_image: str
    commit_sha: str


class IssueClient(Protocol):
    """Small GitHub issue surface kept injectable for network-free tests."""

    def search_issues(self, marker: str) -> list[dict[str, Any]]: ...

    def create_issue(self, title: str, body: str) -> dict[str, Any]: ...

    def comment_issue(self, number: int, body: str) -> None: ...

    def reopen_issue(self, number: int) -> None: ...


class GhIssueClient:
    """Issue client backed by the GitHub CLI authenticated by ``GH_TOKEN``."""

    def _run(self, *arguments: str) -> str:
        return subprocess.run(
            ["gh", *arguments], check=True, text=True, stdout=subprocess.PIPE
        ).stdout

    def search_issues(self, marker: str) -> list[dict[str, Any]]:
        output = self._run(
            "issue",
            "list",
            "--state",
            "all",
            "--search",
            marker,
            "--json",
            "number,state,body",
            "--limit",
            "100",
        )
        return json.loads(output)

    def create_issue(self, title: str, body: str) -> dict[str, Any]:
        self._run("issue", "create", "--title", title, "--body", body)
        return {}

    def comment_issue(self, number: int, body: str) -> None:
        self._run("issue", "comment", str(number), "--body", body)

    def reopen_issue(self, number: int) -> None:
        self._run("issue", "reopen", str(number))


def dedupe_key(test_name: str, failure_message: str) -> str:
    """Return a key that separates test identities and stable failure mechanisms."""
    return dedupe_key_for_signature(test_name, normalize_signature(failure_message))


def dedupe_key_for_signature(test_name: str, signature: str) -> str:
    """Return a SHA-256 dedupe key for one test and normalized signature."""
    return hashlib.sha256(f"{test_name}\0{signature}".encode("utf-8")).hexdigest()


def sanitize_excerpt(message: str) -> str:
    """Bound and sanitize untrusted failure output before rendering it in Markdown."""
    sanitized = CONTROL_CHARACTERS.sub("?", message)
    encoded = sanitized.encode("utf-8")
    if len(encoded) <= EXCERPT_LIMIT:
        return sanitized
    return encoded[:EXCERPT_LIMIT].decode("utf-8", errors="ignore")


def find_ledger_incident(
    ledger_path: pathlib.Path, test_name: str, signature: str
) -> str | None:
    """Return the matching immutable ledger incident identifier, if any."""
    try:
        with ledger_path.open("rb") as ledger_file:
            incidents = tomllib.load(ledger_file).get("incident", [])
    except (FileNotFoundError, OSError, tomllib.TOMLDecodeError):
        return None

    expected_signature = f"sha256:{signature}"
    for incident in incidents:
        if (
            isinstance(incident, dict)
            and incident.get("test") == test_name
            and incident.get("signature") == expected_signature
            and isinstance(incident.get("id"), str)
        ):
            return incident["id"]
    return None


def _recurrence_body(
    event: dict[str, Any], context: RunContext, ledger_incident_id: str | None
) -> str:
    excerpt = sanitize_excerpt(str(event.get("failure_excerpt", "")))
    ledger = ledger_incident_id or "No matching ledger incident"
    return "\n".join(
        (
            "### Soak recurrence",
            f"- Run: {context.run_url}",
            f"- OS image: `{context.os_image}`",
            f"- Commit: `{context.commit_sha}`",
            f"- Classification: `{event.get('classification', 'UNKNOWN')}`",
            f"- Attempts: `{event.get('attempt_count', 0)}`",
            f"- Ledger incident: `{ledger}`",
            "",
            "```text",
            excerpt,
            "```",
        )
    )


def update_issue(
    client: IssueClient,
    event: dict[str, Any],
    context: RunContext,
    ledger_path: pathlib.Path = pathlib.Path(".github/flake-ledger.toml"),
) -> str:
    """Create, comment on, or reopen the one issue for a classified failure."""
    test_name = str(event["test_name"])
    message = str(event.get("failure_excerpt", ""))
    signature = str(event.get("signature") or normalize_signature(message))
    key = dedupe_key_for_signature(test_name, signature)
    marker = f"dormant-flake-key: {key}"
    ledger_incident_id = find_ledger_incident(ledger_path, test_name, signature)
    recurrence = _recurrence_body(event, context, ledger_incident_id)
    matches = client.search_issues(marker)

    if not matches:
        body = f"<!-- {marker} -->\n\n{recurrence}"
        client.create_issue(f"[CI flake] {test_name}", body)
        return "created"

    issue = matches[0]
    number = int(issue["number"])
    if str(issue.get("state", "")).lower() == "closed":
        client.reopen_issue(number)
        client.comment_issue(number, recurrence)
        return "reopened"

    client.comment_issue(number, recurrence)
    return "commented"


def update_issues(
    client: IssueClient,
    events: list[dict[str, Any]],
    context: RunContext,
    ledger_path: pathlib.Path = pathlib.Path(".github/flake-ledger.toml"),
) -> list[str]:
    """Update issues for non-clean test records; deliberately never close an issue."""
    return [update_issue(client, event, context, ledger_path) for event in events]


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--run-url", required=True)
    parser.add_argument("--os-image", required=True)
    parser.add_argument("--commit-sha", required=True)
    parser.add_argument(
        "--ledger", type=pathlib.Path, default=pathlib.Path(".github/flake-ledger.toml")
    )
    args = parser.parse_args()

    events = [json.loads(line) for line in sys.stdin if line.strip()]
    context = RunContext(args.run_url, args.os_image, args.commit_sha)
    for action in update_issues(GhIssueClient(), events, context, args.ledger):
        print(action)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
