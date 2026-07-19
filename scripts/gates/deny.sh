#!/usr/bin/env bash
set -euo pipefail

cd "$(git rev-parse --show-toplevel)"
if ! cargo deny --version >/dev/null 2>&1; then
  printf '%s\n' 'Install with: cargo install cargo-deny --locked' >&2
  exit 1
fi
cargo deny check
