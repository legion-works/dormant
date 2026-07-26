#!/usr/bin/env python3
"""Compile changelog fragments into a release entry.

Reads every fragment under changelog.d/, assembles the compiled entry,
and — when --check is given — refuses to proceed if a declared surface
was not updated.  The compiled entry is printed to stdout; no file is
mutated unless --delete-fragments is passed.
"""

from __future__ import annotations

import argparse
import pathlib
import subprocess
import sys
from typing import Any

# Reuse the front-matter parser from the CI check.
# Path manipulation so we can import from the scripts/ci/ directory.
_CI_DIR = pathlib.Path(__file__).resolve().parent / "ci"
sys.path.insert(0, str(_CI_DIR))
import check_changelog_fragment as _chk  # noqa: E402


VALID_KINDS = _chk.VALID_KINDS
parse_front_matter = _chk.parse_front_matter


def _all_fragments(fragment_dir: pathlib.Path) -> list[pathlib.Path]:
    """Return every .md file under *fragment_dir* except README.md."""
    readme = fragment_dir / "README.md"
    return sorted(
        p for p in fragment_dir.glob("*.md") if p != readme
    )


def _bullet_in_readme(root: pathlib.Path, bullet: str) -> bool:
    """True when *bullet* appears as a substring of README.md."""
    readme = root / "README.md"
    try:
        return bullet in readme.read_text(encoding="utf-8")
    except OSError:
        return False


def _docs_changed_since_prev_tag(root: pathlib.Path) -> bool:
    """True when any file under docs/src/ changed since the previous tag."""
    try:
        prev_tag = subprocess.run(
            ["git", "describe", "--tags", "--abbrev=0"],
            cwd=root, check=True, text=True, capture_output=True,
        ).stdout.strip()
    except subprocess.CalledProcessError:
        # No previous tag — treat as "nothing changed" to avoid a
        # false-positive gate on the very first release.
        return False
    try:
        proc = subprocess.run(
            ["git", "diff", "--name-only", f"{prev_tag}..HEAD", "--", "docs/src/"],
            cwd=root, check=True, text=True, capture_output=True,
        )
    except subprocess.CalledProcessError:
        return False
    return bool(proc.stdout.strip())


def _validate_surfaces(
    root: pathlib.Path,
    fragments: list[tuple[pathlib.Path, dict[str, Any], str]],
) -> list[str]:
    """Run the release-time surface checks.

    Returns a list of refusal messages (empty means pass).
    """
    errors: list[str] = []

    for frag_path, fm, _body in fragments:
        # Report repo-relative, matching every checker under scripts/ci/.
        try:
            label = frag_path.relative_to(root)
        except ValueError:
            label = frag_path

        surfaces: list[str] = fm.get("surfaces", [])
        if not isinstance(surfaces, list):
            surfaces = []

        if "readme" in surfaces:
            bullet: Any = fm.get("readme_bullet", "")
            if isinstance(bullet, str) and bullet.strip():
                if not _bullet_in_readme(root, bullet):
                    errors.append(
                        f"{label}: surfaces declares 'readme' but the "
                        f"readme_bullet text is absent from README.md"
                    )
            else:
                errors.append(
                    f"{label}: surfaces declares 'readme' but "
                    "readme_bullet is missing or empty"
                )

        if "chapter" in surfaces:
            if not _docs_changed_since_prev_tag(root):
                errors.append(
                    f"{label}: surfaces declares 'chapter' but no file "
                    "under docs/src/ changed since the previous tag"
                )

    return errors


def _compile_entry(
    fragments: list[tuple[pathlib.Path, dict[str, Any], str]],
) -> str:
    """Compile fragments into a changelog entry string."""
    breaking: list[str] = []
    highlights: list[str] = []
    added: list[str] = []
    changed: list[str] = []
    fixed: list[str] = []
    removed: list[str] = []

    for _frag_path, fm, body in fragments:
        kind: str = fm.get("kind", "")
        detail = _extract_detail(body)

        if kind == "breaking":
            # Body is the migration sentence; use it as the breaking bullet.
            migration = body.strip()
            if migration:
                breaking.append(migration)
            if detail:
                removed.append(detail)

        elif kind == "capability":
            user_line = _extract_user_can_now(body)
            if user_line:
                highlights.append(f"**{user_line}**")
            if detail:
                added.append(detail)

        elif kind == "improvement":
            if detail:
                changed.append(detail)

        elif kind == "fix":
            if detail:
                fixed.append(detail)

    parts: list[str] = []

    if breaking:
        parts.append("### Breaking")
        for item in breaking:
            parts.append(f"- {item}")
        parts.append("")

    has_highlights = bool(highlights)
    if has_highlights:
        parts.append("### Highlights")
        for item in highlights:
            parts.append(item)
        parts.append("")

    if added:
        parts.append("### Added")
        for item in added:
            parts.append(f"- {item}")
        parts.append("")

    if changed:
        parts.append("### Changed")
        for item in changed:
            parts.append(f"- {item}")
        parts.append("")

    if fixed:
        parts.append("### Fixed")
        for item in fixed:
            parts.append(f"- {item}")
        parts.append("")

    if removed:
        parts.append("### Removed")
        for item in removed:
            parts.append(f"- {item}")
        parts.append("")

    return "\n".join(parts).rstrip() + "\n"


def _extract_user_can_now(body: str) -> str:
    """Return the sentence after 'User can now:' on its line."""
    for line in body.splitlines():
        stripped = line.strip()
        if stripped.lower().startswith("user can now:"):
            return stripped[len("user can now:"):].strip()
    return ""


def _extract_detail(body: str) -> str:
    """Return the text after a 'Detail:' marker line."""
    lines = body.splitlines()
    for i, line in enumerate(lines):
        stripped = line.strip()
        if stripped.lower().startswith("detail:"):
            # Content after 'Detail:' on the same line, plus following lines.
            same_line = stripped[len("detail:"):].strip()
            rest = [same_line] if same_line else []
            rest.extend(line.strip() for line in lines[i + 1:] if line.strip())
            return " ".join(rest)
    return ""


def _has_capability(fragments: list[tuple[pathlib.Path, dict[str, Any], str]]) -> bool:
    return any(fm.get("kind") == "capability" for _, fm, _ in fragments)


def _highlights_present(entry: str) -> bool:
    return "### Highlights" in entry


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--check", action="store_true", default=False,
        help="validate surfaces without printing the compiled entry",
    )
    parser.add_argument(
        "--fragment-dir", type=pathlib.Path, default=pathlib.Path("changelog.d"),
        help="directory containing fragment files (default: changelog.d)",
    )
    parser.add_argument(
        "--delete-fragments", action="store_true", default=False,
        help="delete fragment files after compiling (release-commit action)",
    )
    args = parser.parse_args()

    root = pathlib.Path(
        subprocess.run(
            ["git", "rev-parse", "--show-toplevel"],
            check=True, text=True, capture_output=True,
        ).stdout.strip()
    )

    fragment_dir = (root / args.fragment_dir).resolve()
    if not fragment_dir.is_dir():
        print(f"fragment directory not found: {fragment_dir}", file=sys.stderr)
        return 1

    frag_files = _all_fragments(fragment_dir)

    parsed: list[tuple[pathlib.Path, dict[str, Any], str]] = []
    parse_errors: list[str] = []
    for fp in frag_files:
        try:
            text = fp.read_text(encoding="utf-8")
            fm, body = parse_front_matter(text)
            parsed.append((fp, fm, body))
        except (ValueError, OSError) as exc:
            parse_errors.append(f"{fp}: {exc}")

    if parse_errors:
        print("fragment parse errors:", file=sys.stderr)
        for err in parse_errors:
            print(f"- {err}", file=sys.stderr)
        return 1

    # -- Surface validation --
    surface_errors = _validate_surfaces(root, parsed)
    if surface_errors:
        print("surface validation failed:", file=sys.stderr)
        for err in surface_errors:
            print(f"- {err}", file=sys.stderr)
        return 1

    # -- Compile --
    entry = _compile_entry(parsed)

    # -- Highlights required when a capability exists --
    if _has_capability(parsed) and not _highlights_present(entry):
        print(
            "release-prep: a capability fragment exists but the compiled "
            "entry has no Highlights section",
            file=sys.stderr,
        )
        return 1

    if args.check:
        print("release-prep: all checks passed")
        return 0

    print(entry, end="")

    if args.delete_fragments:
        for fp, _fm, _body in parsed:
            fp.unlink()
        print(
            f"release-prep: deleted {len(parsed)} fragment(s)",
            file=sys.stderr,
        )

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
