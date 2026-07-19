#!/usr/bin/env python3
"""Select and stress Rust test targets changed by a revision range."""

import argparse
import dataclasses
import json
import pathlib
import re
import shlex
import shutil
import subprocess
import sys
from collections.abc import Iterable


HUNK = re.compile(r"^@@ -\d+(?:,\d+)? \+\d+(?:,\d+)? @@(?P<context>.*)$")
CRATE_PATH = re.compile(r"^crates/(?P<crate>[^/]+)/(?P<rest>.+)$")
INTEGRATION_TEST = re.compile(r"^crates/(?P<crate>[^/]+)/tests/(?P<name>[^/]+)\.rs$")
SUPPORT_MODULE = re.compile(r"^crates/(?P<crate>[^/]+)/tests/support/.+\.rs$")
TEST_RANGE = re.compile(
    r"#\s*\[\s*(?:[^\]]*::)?test|cfg\s*\(\s*test\s*\)|\bmod\s+tests?\b|\bfn\s+(?:test_[A-Za-z0-9_]*|[A-Za-z0-9_]*_test)\b"
)


class SelectionError(RuntimeError):
    """A changed Rust test path cannot be mapped safely."""


@dataclasses.dataclass(frozen=True, order=True)
class Target:
    """A Cargo test target selected for one nextest invocation."""

    package: str
    kind: str
    name: str
    manifest_path: str | None = None


@dataclasses.dataclass
class Package:
    """Cargo metadata needed to map a crate path to its runnable targets."""

    name: str
    crate_path: str
    targets: list[Target]
    source_paths: dict[str, Target]


@dataclasses.dataclass
class Metadata:
    """Indexed workspace package metadata."""

    packages: dict[str, Package]


@dataclasses.dataclass
class FileChange:
    """One file's old/new names and all zero-context hunk content."""

    old_path: str | None
    new_path: str | None
    hunks: list[list[str]]

    def paths(self) -> set[str]:
        return {path for path in (self.old_path, self.new_path) if path and path != "/dev/null"}

    def is_test_range(self) -> bool:
        return any(TEST_RANGE.search(line) for hunk in self.hunks for line in hunk)


def _strip_diff_prefix(path: str) -> str | None:
    if path == "/dev/null":
        return None
    return path[2:] if path.startswith(("a/", "b/")) else path


def parse_diff(diff: str) -> list[FileChange]:
    """Parse file names and both sides of each unified-zero diff hunk."""
    changes: list[FileChange] = []
    current: FileChange | None = None
    current_hunk: list[str] | None = None

    for line in diff.splitlines():
        if line.startswith("diff --git "):
            parts = shlex.split(line)
            if len(parts) != 4:
                raise SelectionError(f"cannot parse diff header: {line}")
            current = FileChange(_strip_diff_prefix(parts[2]), _strip_diff_prefix(parts[3]), [])
            changes.append(current)
            current_hunk = None
        elif current is None:
            continue
        elif line.startswith("--- "):
            current.old_path = _strip_diff_prefix(line[4:])
        elif line.startswith("+++ "):
            current.new_path = _strip_diff_prefix(line[4:])
        elif match := HUNK.match(line):
            current_hunk = [match.group("context")]
            current.hunks.append(current_hunk)
        elif current_hunk is not None and line.startswith(("+", "-", " ")) and not line.startswith(("+++", "---")):
            current_hunk.append(line[1:])
    return changes


def parse_metadata(raw_metadata: str, root: pathlib.Path) -> Metadata:
    """Index Cargo metadata by crate directory and source target path."""
    packages: dict[str, Package] = {}
    for raw_package in json.loads(raw_metadata)["packages"]:
        manifest = pathlib.Path(raw_package["manifest_path"])
        try:
            crate_path = manifest.parent.relative_to(root).as_posix()
        except ValueError:
            continue
        targets: list[Target] = []
        source_paths: dict[str, Target] = {}
        for raw_target in raw_package["targets"]:
            kinds = set(raw_target["kind"])
            kind = next((candidate for candidate in ("lib", "bin", "test") if candidate in kinds), None)
            if kind is None:
                continue
            target = Target(raw_package["name"], kind, raw_target["name"])
            targets.append(target)
            source = pathlib.Path(raw_target["src_path"])
            try:
                source_paths[source.relative_to(root).as_posix()] = target
            except ValueError:
                continue
        packages[crate_path] = Package(raw_package["name"], crate_path, targets, source_paths)
    return Metadata(packages)


def _package_for_path(path: str, metadata: Metadata) -> Package | None:
    match = CRATE_PATH.match(path)
    if match is None:
        return None
    return metadata.packages.get(f"crates/{match.group('crate')}")


def _source_target(path: str, package: Package) -> Target:
    if target := package.source_paths.get(path):
        if target.kind in {"lib", "bin"}:
            return target
    libraries = [target for target in package.targets if target.kind == "lib"]
    if libraries:
        return libraries[0]
    binaries = [target for target in package.targets if target.kind == "bin"]
    if len(binaries) == 1:
        return binaries[0]
    raise SelectionError(f"{path}: test range has no containing library or unambiguous binary target")


def _integration_target(path: str, metadata: Metadata) -> Target:
    match = INTEGRATION_TEST.match(path)
    if match is None:
        raise AssertionError("integration target lookup requires a top-level integration test path")
    package = metadata.packages.get(f"crates/{match.group('crate')}")
    if package is None:
        raise SelectionError(f"{path}: package is absent from cargo metadata")
    name = match.group("name")
    for target in package.targets:
        if target.kind == "test" and target.name == name:
            return target
    raise SelectionError(f"{path}: integration target {name!r} is absent from cargo metadata")


def _support_targets(path: str, metadata: Metadata) -> Iterable[Target]:
    match = SUPPORT_MODULE.match(path)
    if match is None:
        raise AssertionError("support target lookup requires a support module path")
    package = metadata.packages.get(f"crates/{match.group('crate')}")
    if package is None:
        raise SelectionError(f"{path}: package is absent from cargo metadata")
    targets = [target for target in package.targets if target.kind == "test"]
    if not targets:
        raise SelectionError(f"{path}: package has no integration targets to select")
    return targets


def _is_fixture(path: str) -> bool:
    return path.startswith(".github/fixtures/nextest-policy/")


def _looks_like_test_path(path: str) -> bool:
    return path.endswith(".rs") and (path.startswith("tests/") or "/tests/" in path)


def select_targets(diff: str, metadata: Metadata, platform: str) -> list[Target]:
    """Map changed test ranges and test files to conservative Cargo targets."""
    selected: set[Target] = set()
    for change in parse_diff(diff):
        paths = change.paths()
        if not paths or all(_is_fixture(path) for path in paths):
            continue

        integration_paths = [path for path in paths if INTEGRATION_TEST.match(path)]
        if integration_paths:
            mapped = False
            for path in (change.new_path, change.old_path):
                if path not in integration_paths:
                    continue
                try:
                    selected.add(_integration_target(path, metadata))
                    mapped = True
                    break
                except SelectionError:
                    continue
            if not mapped:
                raise SelectionError(
                    f"{sorted(integration_paths)[0]}: integration target is absent from cargo metadata"
                )
            paths.difference_update(integration_paths)

        for path in sorted(paths):
            if _is_fixture(path):
                continue
            if SUPPORT_MODULE.match(path):
                selected.update(_support_targets(path, metadata))
                continue
            if path.startswith("vendor/ddc-macos/"):
                if change.is_test_range() and platform == "macos":
                    selected.add(Target("ddc-macos", "lib", "ddc-macos", "vendor/ddc-macos/Cargo.toml"))
                continue

            package = _package_for_path(path, metadata)
            if package is not None and "/src/" in f"/{path}" and change.is_test_range():
                selected.add(_source_target(path, package))
                continue
            if path.endswith(".rs") and (change.is_test_range() or _looks_like_test_path(path)):
                raise SelectionError(f"{path}: unmappable Rust test path or test attribute")
    return sorted(selected)


def target_arguments(target: Target) -> list[str]:
    """Return Cargo selector arguments for one metadata target."""
    arguments = ["--manifest-path", target.manifest_path] if target.manifest_path else ["-p", target.package]
    arguments.append(f"--{target.kind}")
    if target.kind != "lib":
        arguments.append(target.name)
    return arguments


def nextest_commands(target: Target, stress_count: int) -> tuple[list[str], list[str]]:
    """Build the list and stress-run commands for one target."""
    selectors = target_arguments(target)
    list_command = ["cargo", "nextest", "list", *selectors, "--all-features", "--profile", "ci"]
    run_command = [
        "cargo",
        "nextest",
        "run",
        *selectors,
        "--all-features",
        "--profile",
        "ci",
        "--retries",
        "0",
        "--flaky-result",
        "fail",
        "--stress-count",
        str(stress_count),
    ]
    return list_command, run_command


def _copy_junit(root: pathlib.Path, target: Target) -> pathlib.Path | None:
    source = root / "target/nextest/ci/junit.xml"
    destination = root / "target/nextest/changed" / f"{target.package}-{target.name}.xml"
    if not source.is_file():
        print(f"{target.package}/{target.name}: nextest did not produce {source}", file=sys.stderr)
        return None
    destination.parent.mkdir(parents=True, exist_ok=True)
    shutil.copy2(source, destination)
    return destination


def run_targets(root: pathlib.Path, targets: list[Target], stress_count: int) -> int:
    """Run every selected target, retaining reports and the first failing status."""
    first_failure = 0
    reports: list[pathlib.Path] = []
    junit = root / "target/nextest/ci/junit.xml"
    for target in targets:
        list_command, run_command = nextest_commands(target, stress_count)
        listed = subprocess.run(list_command, cwd=root, text=True, capture_output=True)
        if listed.returncode != 0:
            print(listed.stdout, end="")
            print(listed.stderr, end="", file=sys.stderr)
            first_failure = first_failure or listed.returncode
            continue
        if not listed.stdout.strip():
            print(f"{target.package}/{target.name}: selected target contains zero tests", file=sys.stderr)
            first_failure = first_failure or 1
            continue

        junit.unlink(missing_ok=True)
        completed = subprocess.run(run_command, cwd=root)
        report = _copy_junit(root, target)
        if report is None:
            first_failure = first_failure or 1
        else:
            reports.append(report)
        first_failure = first_failure or completed.returncode

    classifier = root / "scripts/ci/classify_nextest_junit.py"
    for report in reports:
        classified = subprocess.run([sys.executable, str(classifier), str(report)], cwd=root)
        first_failure = first_failure or classified.returncode
    return first_failure


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--base", required=True, help="base revision")
    parser.add_argument("--head", default="HEAD", help="head revision (default: HEAD)")
    parser.add_argument("--platform", choices=("linux", "macos"), required=True)
    parser.add_argument("--stress-count", type=int, required=True)
    parser.add_argument("--dry-run", action="store_true")
    args = parser.parse_args()
    root = pathlib.Path(__file__).resolve().parents[2]
    try:
        diff = subprocess.run(
            ["git", "diff", "--unified=0", f"{args.base}..{args.head}"],
            cwd=root,
            text=True,
            capture_output=True,
            check=True,
        ).stdout
        metadata = subprocess.run(
            ["cargo", "metadata", "--no-deps", "--format-version", "1"],
            cwd=root,
            text=True,
            capture_output=True,
            check=True,
        ).stdout
        targets = select_targets(diff, parse_metadata(metadata, root), args.platform)
    except (SelectionError, subprocess.CalledProcessError, json.JSONDecodeError) as error:
        print(f"changed-test selection failed: {error}", file=sys.stderr)
        return 2

    print(f"selected {len(targets)} changed Rust tests")
    if args.dry_run:
        for target in targets:
            for command in nextest_commands(target, args.stress_count):
                print(shlex.join(command))
        return 0
    return run_targets(root, targets, args.stress_count)


if __name__ == "__main__":
    raise SystemExit(main())
