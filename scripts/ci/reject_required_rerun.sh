#!/usr/bin/env bash
set -euo pipefail

infra_rerun_label='ci-infra-rerun'
event_path="${GITHUB_EVENT_PATH:-}"

if (( ${GITHUB_RUN_ATTEMPT:-1} > 1 )); then
  if [[ -n "$event_path" && -f "$event_path" ]] \
    && command -v jq >/dev/null 2>&1 \
    && jq -e --arg label "$infra_rerun_label" \
      '.pull_request.labels? // [] | any(.[]; .name == $label)' \
      "$event_path" >/dev/null 2>&1; then
    printf '%s\n' "INFRASTRUCTURE RERUN OVERRIDE: label $infra_rerun_label is present; allowing this rerun for infrastructure failures only."
    exit 0
  fi
  printf '%s\n' 'Same-SHA reruns are diagnostic only; push a new commit and update .github/flake-ledger.toml.'
  exit 1
fi
