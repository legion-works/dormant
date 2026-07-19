#!/usr/bin/env bash
set -euo pipefail

cd "$(git rev-parse --show-toplevel)"
PKG_CONFIG_PATH=/usr/lib/pkgconfig cargo clippy --workspace --all-targets --all-features -- -D warnings -W clippy::pedantic
