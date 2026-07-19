#!/usr/bin/env bash
set -euo pipefail

cd "$(git rev-parse --show-toplevel)"
if ! cargo nextest --version >/dev/null 2>&1; then
  printf '%s\n' 'Install with: cargo install cargo-nextest --locked' >&2
  exit 1
fi
PKG_CONFIG_PATH=/usr/lib/pkgconfig cargo nextest run --profile ci --workspace --all-features
PKG_CONFIG_PATH=/usr/lib/pkgconfig cargo test --workspace --all-features --doc
