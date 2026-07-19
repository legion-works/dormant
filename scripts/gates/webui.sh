#!/usr/bin/env bash
set -euo pipefail

cd "$(git rev-parse --show-toplevel)/crates/dormant-web/webui"
if ! command -v node >/dev/null 2>&1 || ! command -v npm >/dev/null 2>&1; then
  printf '%s\n' 'Install with: sudo apt-get install -y nodejs npm' >&2
  exit 1
fi
npm run lint
npm run build
npx vitest run
