#!/usr/bin/env bash
set -euo pipefail

cd "$(git rev-parse --show-toplevel)"

# Feature selection is overridable so a target that cannot build the full set
# still runs the identical lint contract. The Windows job sets
# `--features web-ui`: dormantd's `render` feature pulls Wayland and libmpv,
# which do not exist there. Everything else about the gate — the workspace
# scope, `--all-targets`, `-D warnings`, and pedantic — is fixed here so no
# caller can quietly weaken it.
read -r -a DORMANT_GATE_FEATURE_FLAGS <<<"${DORMANT_GATE_FEATURES:---all-features}"

PKG_CONFIG_PATH=/usr/lib/pkgconfig cargo clippy --workspace --all-targets \
  "${DORMANT_GATE_FEATURE_FLAGS[@]}" -- -D warnings -W clippy::pedantic
