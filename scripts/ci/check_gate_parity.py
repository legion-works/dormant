#!/usr/bin/env python3
"""Keep CI and Lefthook gate invocations pointed at the shared scripts."""

from __future__ import annotations

import argparse
import pathlib
import re
import sys

import yaml


RAW_GATE_COMMANDS = (
    "cargo fmt --all -- --check",
    "taplo fmt --check",
    re.compile(r"(?<![/\w.-])typos(?:\s|$)"),
    "gitleaks git",
    "cargo clippy",
    "cargo doc --workspace --no-deps",
    "cargo nextest run",
    "cargo test --workspace --all-features --doc",
    "npm run lint",
    "npm run build",
    "npx vitest run",
    "mdbook build docs",
    "cargo deny check",
    "cargo +1.88 check --workspace",
)


def load_yaml(path: pathlib.Path) -> object:
    with path.open(encoding="utf-8") as handle:
        return yaml.safe_load(handle) or {}


def parse_manifest(path: pathlib.Path) -> dict[str, list[str]]:
    required: dict[str, list[str]] = {"ci": [], "pre-commit": [], "pre-push": []}
    for number, raw_line in enumerate(path.read_text(encoding="utf-8").splitlines(), start=1):
        line = raw_line.strip()
        if not line or line.startswith("#"):
            continue
        try:
            target, script = line.split("\t", maxsplit=1)
        except ValueError as error:
            raise ValueError(f"{path}:{number}: expected target<TAB>script") from error
        if target not in required:
            raise ValueError(f"{path}:{number}: unknown target {target!r}")
        if not script.startswith("scripts/gates/") or not script.endswith(".sh"):
            raise ValueError(f"{path}:{number}: invalid gate script {script!r}")
        required[target].append(script)
    return required


def run_commands(value: object) -> list[str]:
    commands: list[str] = []
    if isinstance(value, dict):
        for key, child in value.items():
            if key == "run" and isinstance(child, str):
                commands.append(child)
            else:
                commands.extend(run_commands(child))
    elif isinstance(value, list):
        for child in value:
            commands.extend(run_commands(child))
    return commands


def target_commands(ci: object, lefthook: object) -> dict[str, list[str]]:
    if not isinstance(lefthook, dict):
        raise ValueError("lefthook YAML root must be a mapping")
    return {
        "ci": run_commands(ci),
        "pre-commit": run_commands(lefthook.get("pre-commit", {})),
        "pre-push": run_commands(lefthook.get("pre-push", {})),
    }


def raw_gate_commands(commands: list[str]) -> list[str]:
    duplicates: list[str] = []
    for command in commands:
        for gate_command in RAW_GATE_COMMANDS:
            if isinstance(gate_command, str):
                matches = gate_command in command
                label = gate_command
            else:
                matches = gate_command.search(command) is not None
                label = gate_command.pattern
            if matches and label not in duplicates:
                duplicates.append(label)
    return duplicates


def validate_parity(ci_path: pathlib.Path, lefthook_path: pathlib.Path, manifest_path: pathlib.Path) -> list[str]:
    try:
        required = parse_manifest(manifest_path)
        commands = target_commands(load_yaml(ci_path), load_yaml(lefthook_path))
    except (OSError, ValueError, yaml.YAMLError) as error:
        return [str(error)]

    errors: list[str] = []
    for target, scripts in required.items():
        for script in scripts:
            if not any(script in command for command in commands[target]):
                errors.append(f"{target}: missing required script {script}")
    for target, target_runs in commands.items():
        for command in raw_gate_commands(target_runs):
            errors.append(f"{target}: raw gate command remains in YAML: {command}")
    return errors


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--ci", required=True, type=pathlib.Path)
    parser.add_argument("--lefthook", required=True, type=pathlib.Path)
    parser.add_argument("--manifest", required=True, type=pathlib.Path)
    args = parser.parse_args()

    errors = validate_parity(args.ci, args.lefthook, args.manifest)
    if errors:
        print("gate parity check failed:", file=sys.stderr)
        for error in errors:
            print(f"- {error}", file=sys.stderr)
        return 1
    print("gate parity check passed")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
