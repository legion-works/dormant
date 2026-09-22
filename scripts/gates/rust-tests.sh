#!/usr/bin/env bash
set -euo pipefail

cd "$(git rev-parse --show-toplevel)"
if ! cargo nextest --version >/dev/null 2>&1; then
  printf '%s\n' 'Install with: cargo install cargo-nextest --locked' >&2
  exit 1
fi
# Feature selection is overridable so a target that cannot build the full set
# still runs the identical test contract. The Windows job sets
# `--features web-ui`: dormantd's `render` feature pulls Wayland and libmpv,
# which do not exist there. Everything else — the workspace scope, the `ci`
# profile (retries, flaky-result = fail, fail-fast = false), and the doctest
# pass — is fixed here so no caller can quietly weaken it.
read -r -a DORMANT_GATE_FEATURE_FLAGS <<<"${DORMANT_GATE_FEATURES:---all-features}"

# Harmless where it does not apply: on Windows there is no pkg-config and
# nothing reads this, but keeping one command line for both platforms is worth
# more than branching on the host.
PKG_CONFIG_PATH=/usr/lib/pkgconfig cargo nextest run --profile ci --workspace \
  "${DORMANT_GATE_FEATURE_FLAGS[@]}"
PKG_CONFIG_PATH=/usr/lib/pkgconfig cargo test --workspace \
  "${DORMANT_GATE_FEATURE_FLAGS[@]}" --doc
