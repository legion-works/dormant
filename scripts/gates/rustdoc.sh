#!/usr/bin/env bash
set -euo pipefail

cd "$(git rev-parse --show-toplevel)"
RUSTDOCFLAGS="-D warnings" PKG_CONFIG_PATH=/usr/lib/pkgconfig cargo doc --workspace --no-deps --all-features
