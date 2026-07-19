#!/usr/bin/env bash
set -euo pipefail

cd "$(git rev-parse --show-toplevel)"
if ! command -v gitleaks >/dev/null 2>&1; then
  printf '%s\n' 'Install with: go install github.com/gitleaks/gitleaks/v8@v8.24.2' >&2
  exit 1
fi

case "${1:-}" in
  staged)
    gitleaks git --staged --no-banner --redact
    ;;
  history)
    gitleaks git --no-banner --redact
    ;;
  *)
    printf '%s\n' 'Usage: scripts/gates/gitleaks.sh {staged|history}' >&2
    exit 2
    ;;
esac
