#!/usr/bin/env bash
set -euo pipefail

cd "$(git rev-parse --show-toplevel)"

patterns=()
while [[ $# -gt 0 && "$1" != "--" ]]; do
  if [[ "$1" != "--pattern" || $# -lt 2 ]]; then
    printf '%s\n' 'Usage: scripts/gates/changed-files.sh --pattern GLOB [--pattern GLOB] -- command' >&2
    exit 2
  fi
  patterns+=("$2")
  shift 2
done

if [[ $# -eq 0 || "$1" != "--" || ${#patterns[@]} -eq 0 ]]; then
  printf '%s\n' 'Usage: scripts/gates/changed-files.sh --pattern GLOB [--pattern GLOB] -- command' >&2
  exit 2
fi
shift

base=""
upstream="$(git rev-parse --abbrev-ref --symbolic-full-name '@{upstream}' 2>/dev/null || true)"
if [[ -n "$upstream" ]]; then
  base="$(git merge-base HEAD "$upstream" 2>/dev/null || true)"
else
  push_line="$(cat)"
  if [[ -n "$push_line" ]]; then
    read -r _local_ref _local_sha remote_ref remote_sha <<<"$push_line"
    if [[ "$remote_sha" != "0000000000000000000000000000000000000000" ]]; then
      base="$(git merge-base HEAD "$remote_sha" 2>/dev/null || true)"
    fi
    if [[ -z "$base" && -n "${remote_ref:-}" ]]; then
      base="$(git merge-base HEAD "$remote_ref" 2>/dev/null || true)"
    fi
  fi
fi

if [[ -z "$base" ]]; then
  printf '%s\n' 'Changed-file base is unknown; running gate.' >&2
  exec "$@"
fi

if ! changed="$(git diff --name-only "$base"...HEAD)"; then
  printf '%s\n' 'Changed-file diff is unavailable; running gate.' >&2
  exec "$@"
fi

while IFS= read -r path; do
  for pattern in "${patterns[@]}"; do
    # shellcheck disable=SC2053 # Gate patterns are intentionally shell globs.
    if [[ "$path" == $pattern ]]; then
      exec "$@"
    fi
  done
done <<<"$changed"
