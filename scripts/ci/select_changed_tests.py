#!/usr/bin/env python3
"""Select and stress Rust test targets changed by a revision range."""

import argparse
import dataclasses
import json
import os
import pathlib
import re
import shlex
import shutil
import subprocess
import sys
from collections.abc import Iterable, Mapping


HUNK = re.compile(r"^@@ -\d+(?:,\d+)? \+\d+(?:,\d+)? @@(?P<context>.*)$")
CRATE_PATH = re.compile(r"^crates/(?P<crate>[^/]+)/(?P<rest>.+)$")
INTEGRATION_TEST = re.compile(r"^crates/(?P<crate>[^/]+)/tests/(?P<name>[^/]+)\.rs$")
SUPPORT_MODULE = re.compile(r"^crates/(?P<crate>[^/]+)/tests/support/.+\.rs$")
TEST_RANGE = re.compile(
    r"#\s*\[\s*(?:[^\]]*::)?test|cfg\s*\(\s*test\s*\)|\bmod\s+tests?\b|\bfn\s+(?:test_[A-Za-z0-9_]*|[A-Za-z0-9_]*_test)\b"
)
MODULE = re.compile(r"(?m)^\s*(?:pub\s*(?:\([^)]*\))?\s+)?mod\s+(?P<name>[A-Za-z_][A-Za-z0-9_]*)\s*;")
TOP_LEVEL_CFG = re.compile(r"(?m)^\s*#!\s*\[\s*cfg\s*\((?P<expression>.+)\)\s*\]")
TARGET_OS = re.compile(r'target_os\s*=\s*"(?P<name>[A-Za-z0-9_]+)"')


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


def _source_text(path: str, source_files: Mapping[str, tuple[str | None, str | None]] | None) -> str | None:
    if source_files is None or path not in source_files:
        return None
    old, new = source_files[path]
    return new if new is not None else old


def _module_paths(parent: str, name: str) -> tuple[str, str]:
    parent_path = pathlib.PurePosixPath(parent)
    directory = parent_path.parent
    if parent_path.name not in {"lib.rs", "main.rs", "mod.rs"}:
        directory /= parent_path.stem
    return ((directory / f"{name}.rs").as_posix(), (directory / name / "mod.rs").as_posix())


def _is_reachable(root: str, target: str, source_files: Mapping[str, tuple[str | None, str | None]]) -> bool:
    pending = [root]
    visited: set[str] = set()
    while pending:
        current = pending.pop()
        if current in visited:
            continue
        visited.add(current)
        if current == target:
            return True
        source = _source_text(current, source_files)
        if source is None:
            continue
        for module in MODULE.finditer(source):
            for child in _module_paths(current, module.group("name")):
                if child == target:
                    return True
                if child in source_files:
                    pending.append(child)
    return False


def _source_targets(
    path: str,
    package: Package,
    source_files: Mapping[str, tuple[str | None, str | None]] | None,
) -> list[Target]:
    if target := package.source_paths.get(path):
        if target.kind in {"lib", "bin"}:
            return [target]
    candidates = [target for target in package.targets if target.kind in {"lib", "bin"}]
    if not candidates:
        raise SelectionError(f"{path}: test range has no containing library or binary target")
    if source_files is None or len(candidates) == 1:
        return candidates
    roots = {target: root for root, target in package.source_paths.items() if target in candidates}
    owners = [target for target, root in roots.items() if _is_reachable(root, path, source_files)]
    return owners or candidates


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


def _source_contains_test_code(change: FileChange, source_files: Mapping[str, tuple[str | None, str | None]] | None) -> bool:
    if source_files is None:
        return True
    return any(
        source is not None and TEST_RANGE.search(source) is not None
        for path in change.paths()
        for source in source_files.get(path, (None, None))
    )


def _platform_compatible(source: str | None, platform: str) -> bool:
    if source is None:
        return True
    expressions = [match.group("expression") for match in TOP_LEVEL_CFG.finditer(source)]
    operating_systems = {name for expression in expressions for name in TARGET_OS.findall(expression)}
    if not operating_systems:
        return True
    if any("not(" in expression.replace(" ", "") for expression in expressions):
        return platform not in operating_systems
    return platform in operating_systems


def _new_source_for(change: FileChange, path: str, source_files: Mapping[str, tuple[str | None, str | None]] | None) -> str | None:
    if source_files is None:
        return None
    old, new = source_files.get(path, (None, None))
    if path == change.new_path:
        return new
    return old


def select_targets(
    diff: str,
    metadata: Metadata,
    platform: str,
    source_files: Mapping[str, tuple[str | None, str | None]] | None = None,
) -> list[Target]:
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
                if not _platform_compatible(_new_source_for(change, path, source_files), platform):
                    mapped = True
                    break
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
            if package is not None and "/src/" in f"/{path}" and (
                change.is_test_range() or _source_contains_test_code(change, source_files)
            ):
                selected.update(_source_targets(path, package, source_files))
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


def nextest_commands(
    target: Target,
    stress_count: int,
    repo_root: pathlib.Path | None = None,
) -> tuple[list[str], list[str], dict[str, str]]:
    """Build the list and stress-run commands for one target.

    Returns the commands plus the extra environment they need. A target with a
    ``manifest_path`` lives in a separate workspace (the vendored ``ddc-macos``
    fork, excluded from the root workspace) that has no ``.config/nextest.toml``
    and its own ``target/`` dir: point nextest at the repo config so ``--profile
    ci`` resolves, and build into the repo's shared ``target/`` so the JUnit
    report lands where ``_copy_junit`` expects it.
    """
    selectors = target_arguments(target)
    extra: list[str] = []
    env: dict[str, str] = {}
    if target.manifest_path is not None and repo_root is not None:
        repo_config = repo_root / ".config" / "nextest.toml"
        if repo_config.is_file():
            extra += ["--config-file", str(repo_config)]
        # Do NOT set CARGO_TARGET_DIR: nextest resolves the JUnit report path
        # relative to the manifest's workspace root (ignoring CARGO_TARGET_DIR),
        # so redirecting build artifacts to the repo target/ would split the
        # report away from where _copy_junit (now manifest-root-aware) looks.
        # Letting both land in the separate workspace's own target/ keeps them
        # together and avoids a cross-workdir copy race.
    list_command = [
        "cargo",
        "nextest",
        "list",
        *selectors,
        "--all-features",
        "--profile",
        "ci",
        *extra,
    ]
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
        *extra,
    ]
    return list_command, run_command, env


def _manifest_workspace_root(root: pathlib.Path, target: Target) -> pathlib.Path:
    """Workspace root nextest resolves JUnit paths against.

    For a workspace target (no ``manifest_path``) that is the repo root. For a
    target run via ``--manifest-path`` into a separate workspace (the vendored
    ``ddc-macos`` fork, excluded from the root workspace) nextest resolves its
    ``junit.path`` relative to *that* manifest's workspace root, not
    ``CARGO_TARGET_DIR`` — so the report lands in the separate workspace's
    ``target/``, and ``_copy_junit`` must look there.
    """
    if target.manifest_path is None:
        return root
    return (root / target.manifest_path).resolve().parent


def junit_destination(root: pathlib.Path, target: Target) -> pathlib.Path:
    """Return a per-kind JUnit destination that cannot collide within a package."""
    return root / "target/nextest/changed" / f"{target.package}-{target.kind}-{target.name}.xml"


def _copy_junit(root: pathlib.Path, target: Target) -> pathlib.Path | None:
    source = _manifest_workspace_root(root, target) / "target/nextest/ci/junit.xml"
    destination = junit_destination(root, target)
    if not source.is_file():
        print(f"{target.package}/{target.name}: nextest did not produce {source}", file=sys.stderr)
        return None
    destination.parent.mkdir(parents=True, exist_ok=True)
    shutil.copy2(source, destination)
    return destination


def load_source_files(
    root: pathlib.Path,
    base: str,
    head: str,
    diff: str,
    metadata: Metadata | None = None,
) -> dict[str, tuple[str | None, str | None]]:
    """Load both revisions of changed Rust files for conservative test ownership."""
    sources: dict[str, tuple[str | None, str | None]] = {}

    def show(revision: str, path: str) -> str | None:
        completed = subprocess.run(
            ["git", "show", f"{revision}:{path}"], cwd=root, text=True, capture_output=True
        )
        return completed.stdout if completed.returncode == 0 else None

    for change in parse_diff(diff):
        if change.old_path and change.old_path.endswith(".rs"):
            sources[change.old_path] = (show(base, change.old_path), None)
        if change.new_path and change.new_path.endswith(".rs"):
            old = sources.get(change.new_path, (None, None))[0]
            sources[change.new_path] = (old, show(head, change.new_path))

    # Ownership resolution walks `mod` chains from crate roots (lib.rs/main.rs/mod.rs).
    # Those roots are rarely in the diff, so load them from HEAD too — otherwise a
    # changed non-root file cannot be mapped to its owning lib/bin target and the
    # selector falls back to every candidate (e.g. selecting a zero-test --bin
    # alongside the --lib that actually owns the tests).
    if metadata is not None:
        for package in metadata.packages.values():
            for root_path in package.source_paths:
                if root_path.endswith(".rs") and root_path not in sources:
                    sources[root_path] = (None, show(head, root_path))
    return sources


def run_targets(root: pathlib.Path, targets: list[Target], stress_count: int) -> int:
    """Run every selected target, retaining reports and the first failing status."""
    first_failure = 0
    reports: list[pathlib.Path] = []
    for target in targets:
        list_command, run_command, env = nextest_commands(target, stress_count, root)
        run_env = {**os.environ, **env} if env else None
        listed = subprocess.run(list_command, cwd=root, text=True, capture_output=True, env=run_env)
        if listed.returncode != 0:
            print(listed.stdout, end="")
            print(listed.stderr, end="", file=sys.stderr)
            first_failure = first_failure or listed.returncode
            continue
        if not listed.stdout.strip():
            print(f"{target.package}/{target.name}: selected target contains zero tests", file=sys.stderr)
            first_failure = first_failure or 1
            continue

        junit = _manifest_workspace_root(root, target) / "target/nextest/ci/junit.xml"
        junit.unlink(missing_ok=True)
        completed = subprocess.run(run_command, cwd=root, env=run_env)
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
        parsed_metadata = parse_metadata(metadata, root)
        targets = select_targets(
            diff,
            parsed_metadata,
            args.platform,
            load_source_files(root, args.base, args.head, diff, parsed_metadata),
        )
    except (SelectionError, subprocess.CalledProcessError, json.JSONDecodeError) as error:
        print(f"changed-test selection failed: {error}", file=sys.stderr)
        return 2

    print(f"selected {len(targets)} changed Rust tests")
    if args.dry_run:
        for target in targets:
            for command in nextest_commands(target, args.stress_count, root)[:2]:
                print(shlex.join(command))
        return 0
    return run_targets(root, targets, args.stress_count)


if __name__ == "__main__":
    raise SystemExit(main())
