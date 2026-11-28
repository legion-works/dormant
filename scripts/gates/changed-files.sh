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
gate=("$@")
changed=()
multiple_pushed_shas=false
unset DORMANT_PUSHED_SHA
list="$(mktemp)"
trap 'rm -f -- "$list"' EXIT

run_gate() {
  if [[ $# -gt 0 ]]; then
    printf '%s\n' "$1" >&2
  fi
  if [[ "$multiple_pushed_shas" == true ]]; then
    printf '%s\n' 'Warning: push has multiple distinct local SHAs; running gate on the working tree.' >&2
  fi
  rm -f -- "$list"
  exec "${gate[@]}"
}

# Add the paths of a diff to `changed`. The arguments are the diff endpoints:
# `<old> <new>` compares two trees, `<base>...<new>` compares <new> with the
# merge-base. -z gives raw paths (no quoting), and --no-renames lists both
# names of a rename. A failed diff runs the gate.
add_changes() {
  if ! git diff --no-renames --name-only -z "$@" >"$list"; then
    run_gate 'Changed-file diff is unavailable; running gate.'
  fi
  local path
  while IFS= read -r -d '' path; do
    changed+=("$path")
  done <"$list"
}

push_lines=""
if [[ ! -t 0 ]]; then
  push_lines="$(cat)"
fi

if [[ -n "$push_lines" ]]; then
  # git sends one line for each pushed ref: <local ref> <local sha> <remote
  # ref> <remote sha>. Check the pushed commit of every ref, not HEAD.
  # Determine the SHA before a failed diff can run the gate early.
  pushed_sha=""
  while read -r _local_ref local_sha _remote_ref _remote_sha; do
    if [[ -z "${local_sha:-}" || "$local_sha" == "$zero" ]]; then
      continue
    fi
    if [[ -n "$pushed_sha" && "$local_sha" != "$pushed_sha" ]]; then
      multiple_pushed_shas=true
      break
    fi
    pushed_sha="$local_sha"
  done <<<"$push_lines"
  if [[ -n "$pushed_sha" && "$multiple_pushed_shas" == false ]]; then
    export DORMANT_PUSHED_SHA="$pushed_sha"
  fi

  while read -r _local_ref local_sha _remote_ref remote_sha; do
    if [[ -z "${local_sha:-}" || "$local_sha" == "$zero" ]]; then
      # A deletion pushes no commit.
      continue
    fi
    if [[ "$remote_sha" != "$zero" ]]; then
      # An existing ref: compare the old and the new remote tree, so a forced
      # update also shows the paths that it removes. If this clone cannot
      # read the old commit, the change is unknown.
      if ! git cat-file -e "$remote_sha^{commit}" 2>/dev/null; then
        run_gate 'Old remote commit is not available; running gate.'
      fi
      add_changes "$remote_sha" "$local_sha"
      continue
    fi
    # A new ref: compare with the integration branch that pull requests
    # target. If that ref is missing, the base is unknown.
    if ! git rev-parse --verify --quiet refs/remotes/origin/dev >/dev/null; then
      run_gate 'Changed-file base is unknown; running gate.'
    fi
    add_changes refs/remotes/origin/dev..."$local_sha"
  done <<<"$push_lines"
else
  # A run outside a push compares HEAD with its upstream.
  upstream="$(git rev-parse --abbrev-ref --symbolic-full-name '@{upstream}' 2>/dev/null || true)"
  if [[ -z "$upstream" ]]; then
    run_gate 'Changed-file base is unknown; running gate.'
  fi
  add_changes "$upstream"...HEAD
fi

# The guarded expansion keeps an empty list valid under `set -u` on bash 3.2.
for path in ${changed[@]+"${changed[@]}"}; do
  for pattern in "${patterns[@]}"; do
    # shellcheck disable=SC2053 # Gate patterns are intentionally shell globs.
    if [[ "$path" == $pattern ]]; then
      run_gate
    fi
  done
done
