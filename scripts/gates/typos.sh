#!/usr/bin/env bash
set -euo pipefail

cd "$(git rev-parse --show-toplevel)"
if ! command -v typos >/dev/null 2>&1; then
  printf '%s\n' 'Install with: cargo install typos-cli --locked' >&2
  exit 1
fi
typos
