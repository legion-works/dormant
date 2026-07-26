#!/usr/bin/env python3
"""Validate changelog fragments for feat/fix pull requests.

On every `feat:` or `fix:` pull request, at least one new file must be
added under changelog.d/.  Every fragment must carry valid front matter
and meet the content rules in docs/DOCS-STANDARD.md.

The `skip-changelog` label bypasses the gate when the PR cannot carry a
meaningful user-facing sentence (refactors, CI-only, doc-only).
"""

from __future__ import annotations

import argparse
import pathlib
import re
import subprocess
import sys
from typing import Any

VALID_KINDS: frozenset[str] = frozenset({"capability", "improvement", "fix", "breaking"})

# Conventional-commit type prefix, with optional scope: feat(core): … / fix: …
FEAT_FIX_RE = re.compile(r"^(feat|fix)(?:\([^)]*\))?\s*:", re.IGNORECASE)

# The directory's own README is not a fragment.
_README_RELPATH = "changelog.d/README.md"


def _split_front_matter(text: str) -> tuple[list[str], str]:
    """Return (front-matter lines, body) split on --- delimiters."""
    lines = text.splitlines()
    if not lines or lines[0].strip() != "---":
        raise ValueError("front matter must start with ---")
    end_idx: int | None = None
    for i in range(1, len(lines)):
        if lines[i].strip() == "---":
            end_idx = i
            break
    if end_idx is None:
        raise ValueError("front matter must end with ---")
    return lines[1:end_idx], "\n".join(lines[end_idx + 1:])


def _parse_scalar(raw: str) -> str:
    """Strip surrounding quotes from a YAML-ish scalar value."""
    s = raw.strip()
    if len(s) >= 2 and s[0] == s[-1] and s[0] in ('"', "'"):
        return s[1:-1]
    return s


def _parse_value(raw: str) -> Any:
    """Parse a YAML-ish value: list, quoted string, or plain scalar."""
    s = raw.strip()
    if s.startswith("[") and s.endswith("]"):
        inner = s[1:-1].strip()
        if not inner:
            return []
        return [_parse_scalar(item) for item in inner.split(",")]
    if s.startswith('"') or s.startswith("'"):
        return _parse_scalar(s)
    return s


def parse_front_matter(text: str) -> tuple[dict[str, Any], str]:
    """Parse YAML-ish front matter delimited by --- lines.

    Returns (front_matter_dict, body_text).  Raises ValueError on
    malformed input.
    """
    fm_lines, body = _split_front_matter(text)
    result: dict[str, Any] = {}
    for raw_line in fm_lines:
        line = raw_line.strip()
        if not line:
            continue
        if ": " not in line:
            if line.endswith(":"):
                result[line[:-1].strip()] = ""
                continue
            raise ValueError(f"invalid front matter line: {raw_line!r}")
        key, rest = line.split(": ", 1)
        key = key.strip()
        result[key] = _parse_value(rest)
    return result, body


def _has_user_can_now(body: str) -> bool:
    """True when body contains a non-empty 'User can now:' line."""
    for line in body.splitlines():
        stripped = line.strip()
        if stripped.lower().startswith("user can now:") and len(stripped) > len("user can now:"):
            return True
    return False


def _is_title_feat_or_fix(pr_title: str | None) -> bool:
    """True when the PR title starts with feat: or fix: (with optional scope)."""
    if pr_title is None:
        return False
    return FEAT_FIX_RE.match(pr_title) is not None


def validate_fragment(root: pathlib.Path, rel_path: str) -> list[str]:
    """Validate a single fragment file.  Returns a list of error strings.

    A fragment that exists in the revision range but is absent from the
    worktree was consumed by a release — skip it silently rather than
    treating it as broken.
    """
    errors: list[str] = []
    label = rel_path
    full_path = root / rel_path

    try:
        text = full_path.read_text(encoding="utf-8")
    except FileNotFoundError:
        # Fragment was added in the range and deleted before HEAD —
        # consumed by a release commit.  It was already validated
        # when it was originally added.
        return []
    except OSError as exc:
        return [f"{label}: cannot read file: {exc}"]

    try:
        fm, body = parse_front_matter(text)
    except ValueError as exc:
        return [f"{label}: {exc}"]

    # -- kind --
    kind: Any = fm.get("kind")
    if not isinstance(kind, str) or not kind.strip():
        errors.append(f"{label}: kind is required")
        kind = ""
    elif kind not in VALID_KINDS:
        errors.append(
            f"{label}: kind must be one of {sorted(VALID_KINDS)}, got {kind!r}"
        )

    # -- surfaces --
    surfaces: Any = fm.get("surfaces")
    if not isinstance(surfaces, list):
        errors.append(f"{label}: surfaces must be a list (got {type(surfaces).__name__})")
        surfaces = []

    if kind == "capability":
        if not surfaces:
            errors.append(f"{label}: capability must declare at least one surface")
        if not _has_user_can_now(body):
            errors.append(f"{label}: capability requires a 'User can now:' line")

    # -- readme_bullet when surfaces includes readme --
    if "readme" in surfaces:
        bullet: Any = fm.get("readme_bullet")
        if not isinstance(bullet, str) or not bullet.strip():
            errors.append(
                f"{label}: surfaces includes 'readme' but "
                "readme_bullet is missing or empty"
            )

    # -- breaking requires migration text --
    if kind == "breaking":
        if not body.strip():
            errors.append(
                f"{label}: breaking change requires a migration sentence in the body"
            )

    return errors


def _find_new_fragment_relpaths(
    root: pathlib.Path, revision_range: str,
) -> list[str]:
    """Return repo-relative paths of fragments added under changelog.d/
    within *revision_range*."""
    try:
        proc = subprocess.run(
            [
                "git", "diff", "--name-only", "--diff-filter=A",
                revision_range, "--", "changelog.d/",
            ],
            cwd=root,
            check=True,
            text=True,
            capture_output=True,
        )
    except subprocess.CalledProcessError:
        # Base unavailable — the caller's bash wrapper already handles
        # skipping; if we reach this point the range was provided so
        # surface the error.
        raise
    paths: list[str] = []
    for line in proc.stdout.splitlines():
        p = line.strip()
        if not p:
            continue
        if p == _README_RELPATH:
            continue
        paths.append(p)
    return paths


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--range", required=True, dest="revision_range",
        help="git revision range, e.g. origin/dev...HEAD",
    )
    parser.add_argument(
        "--pr-title", default=None,
        help="PR title; when present and feat/fix, a fragment is required",
    )
    parser.add_argument(
        "--skip-label", action="store_true", default=False,
        help="skip-changelog label is present on this PR",
    )
    args = parser.parse_args()

    root = pathlib.Path(
        subprocess.run(
            ["git", "rev-parse", "--show-toplevel"],
            check=True, text=True, capture_output=True,
        ).stdout.strip()
    )

    fragment_relpaths = _find_new_fragment_relpaths(root, args.revision_range)

    # Validate every fragment that still exists in the worktree.
    # Fragments consumed by a release (added in the range, deleted
    # before HEAD) are skipped silently — they were already validated
    # when they were originally added.
    all_errors: list[str] = []
    for rel_path in fragment_relpaths:
        all_errors.extend(validate_fragment(root, rel_path))

    # Fragment-existence check: only when the PR title is feat/fix and
    # the skip-changelog label is NOT present.
    enforce = _is_title_feat_or_fix(args.pr_title) and not args.skip_label
    if enforce and not fragment_relpaths:
        all_errors.append(
            "feat/fix pull request has no changelog fragment; "
            "add one under changelog.d/ or apply the skip-changelog label"
        )

    if all_errors:
        print("changelog fragment check failed:", file=sys.stderr)
        for err in all_errors:
            print(f"- {err}", file=sys.stderr)
        return 1

    print("changelog fragment check passed")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
