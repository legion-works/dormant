#!/usr/bin/env bash
set -euo pipefail

cd "$(git rev-parse --show-toplevel)/crates/dormant-web/webui"
if ! command -v node >/dev/null 2>&1 || ! command -v npm >/dev/null 2>&1; then
  printf '%s\n' 'Install with: sudo apt-get install -y nodejs npm' >&2
  exit 1
fi

# npm run build overwrites the tracked 101-byte stub at dist/index.html
# with a real Vite artifact. Save the current content so the gate never
# dirties the working tree — the trap fires on every exit path (success,
# failure, interrupt), preserves the original exit code, and cleans up.
PLACEHOLDER_BAK="$(mktemp)"
cp dist/index.html "$PLACEHOLDER_BAK"

cleanup() {
  local rc=$?
  cp "$PLACEHOLDER_BAK" dist/index.html
  rm -f "$PLACEHOLDER_BAK"
  exit $rc
}
trap cleanup EXIT

npm run lint
npm run build
npx vitest run
