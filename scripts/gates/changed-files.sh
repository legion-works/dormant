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
    read -r _local_ref _local_sha _remote_ref remote_sha <<<"$push_line"
    if [[ "$remote_sha" != "0000000000000000000000000000000000000000" ]]; then
      base="$(git merge-base HEAD "$remote_sha" 2>/dev/null || true)"
    fi
    # The pushed remote ref names a ref on the remote. Resolved in this
    # repository, `refs/heads/<branch>` is the local branch being pushed, so its
    # merge-base is HEAD and every gate would skip. For a branch the remote
    # does not have yet, compare with the integration branch that pull
    # requests target; if that ref is missing, the base stays unknown and
    # the gate runs.
    if [[ -z "$base" ]] && git rev-parse --verify --quiet refs/remotes/origin/dev >/dev/null; then
      base="$(git merge-base HEAD refs/remotes/origin/dev 2>/dev/null || true)"
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
