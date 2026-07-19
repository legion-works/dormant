#!/usr/bin/env bash
set -euo pipefail

cd "$(git rev-parse --show-toplevel)"
if ! cargo +1.88 --version >/dev/null 2>&1; then
  printf '%s\n' 'Install with: rustup toolchain install 1.88' >&2
  exit 1
fi
PKG_CONFIG_PATH=/usr/lib/pkgconfig cargo +1.88 check --workspace
