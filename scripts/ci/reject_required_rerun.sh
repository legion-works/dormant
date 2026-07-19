#!/usr/bin/env bash
set -euo pipefail

if (( ${GITHUB_RUN_ATTEMPT:-1} > 1 )); then
  printf '%s\n' 'Same-SHA reruns are diagnostic only; push a new commit and update .github/flake-ledger.toml.'
  exit 1
fi
