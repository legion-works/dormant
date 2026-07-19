#!/usr/bin/env bash
set -euo pipefail

if ! command -v python3 >/dev/null 2>&1; then
  printf '%s\n' 'Install with: sudo apt-get install -y python3' >&2
  exit 1
fi

cd "$(git rev-parse --show-toplevel)"
staged_tree="$(git write-tree)"
python3 scripts/ci/check_test_timing.py --range "HEAD..$staged_tree"
