#!/usr/bin/env bash
set -euo pipefail

cd "$(git rev-parse --show-toplevel)"
staged_tree="$(git write-tree)"
python3 scripts/ci/check_test_timing.py --range "HEAD..$staged_tree"
