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

zero=0000000000000000000000000000000000000000
changed=""

# Add the files that `commit` changes since `base` to `changed`. An unknown
# base or a failed diff runs the gate.
add_changes() {
  local base="$1" commit="$2" files
  if [[ -z "$base" ]]; then
    printf '%s\n' 'Changed-file base is unknown; running gate.' >&2
    exec "${gate[@]}"
  fi
  if ! files="$(git diff --name-only "$base"..."$commit")"; then
    printf '%s\n' 'Changed-file diff is unavailable; running gate.' >&2
    exec "${gate[@]}"
  fi
  changed+="$files"$'\n'
}

gate=("$@")
push_lines=""
if [[ ! -t 0 ]]; then
  push_lines="$(cat)"
fi

if [[ -n "$push_lines" ]]; then
  # git sends one line for each pushed ref: <local ref> <local sha> <remote
  # ref> <remote sha>. Check the pushed commit of every ref, not HEAD.
  while read -r _local_ref local_sha _remote_ref remote_sha; do
    if [[ -z "${local_sha:-}" || "$local_sha" == "$zero" ]]; then
      # A deletion pushes no commit.
      continue
    fi
    base=""
    if [[ "$remote_sha" != "$zero" ]]; then
      base="$(git merge-base "$local_sha" "$remote_sha" 2>/dev/null || true)"
    fi
    # The pushed remote ref names a ref on the remote. Resolved in this
    # repository, `refs/heads/<branch>` is the local branch being pushed, so its
    # merge-base is the pushed commit and every gate would skip. For a branch
    # the remote does not have yet, compare with the integration branch that
    # pull requests target; if that ref is missing, the base stays unknown and
    # the gate runs.
    if [[ -z "$base" ]] && git rev-parse --verify --quiet refs/remotes/origin/dev >/dev/null; then
      base="$(git merge-base "$local_sha" refs/remotes/origin/dev 2>/dev/null || true)"
    fi
    add_changes "$base" "$local_sha"
  done <<<"$push_lines"
else
  # A run outside a push compares HEAD with its upstream.
  upstream="$(git rev-parse --abbrev-ref --symbolic-full-name '@{upstream}' 2>/dev/null || true)"
  base=""
  if [[ -n "$upstream" ]]; then
    base="$(git merge-base HEAD "$upstream" 2>/dev/null || true)"
  fi
  add_changes "$base" HEAD
fi

while IFS= read -r path; do
  for pattern in "${patterns[@]}"; do
    # shellcheck disable=SC2053 # Gate patterns are intentionally shell globs.
    if [[ "$path" == $pattern ]]; then
      exec "$@"
    fi
  done
done <<<"$changed"
