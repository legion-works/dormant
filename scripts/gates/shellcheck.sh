#!/usr/bin/env bash
set -euo pipefail

# Lints every tracked shell script. The repo's gates, the release helpers, and
# the local deploy script are all bash, and until this gate existed nothing
# checked any of them -- `bash -n` catches syntax errors and nothing else.
#
# The bug that motivated it: `set -o pipefail` with `strings "$BIN" | grep -q`,
# where grep exits on its first match and strings dies of SIGPIPE, failing the
# script on a successful check. Valid syntax, correct-looking, wrong. That is
# SC2312-adjacent territory and exactly what a linter is for.
#
# Severity is `warning`: the whole tree passes at that level today, so the gate
# starts green and every future finding is a real regression rather than a
# backlog to triage. Raising it to `style` later is a deliberate choice, not
# something to slip in.

cd "$(git rev-parse --show-toplevel)"

if ! command -v shellcheck >/dev/null 2>&1; then
  printf '%s\n' 'Install with: apt install shellcheck (or brew install shellcheck)' >&2
  exit 1
fi

# NUL-delimited so a path containing whitespace cannot split into two arguments.
mapfile -t -d '' scripts < <(git ls-files -z '*.sh')

if [ ${#scripts[@]} -eq 0 ]; then
  printf '%s\n' 'shellcheck: no tracked shell scripts found -- the glob is wrong' >&2
  exit 1
fi

printf 'shellcheck: %d scripts\n' "${#scripts[@]}"
shellcheck --severity=warning "${scripts[@]}"
