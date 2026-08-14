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

# Exit 2, distinct from shellcheck's own exit 1. A broken check and a clean
# result must not share an exit code -- otherwise the one byte a caller reads
# cannot tell "nothing to report" from "this gate no longer works".
if [ ${#scripts[@]} -eq 0 ]; then
  printf '%s\n' 'SCOPE FAILURE: shellcheck found no tracked shell scripts -- the glob is wrong' >&2
  exit 2
fi

# Floor set below the current count so ordinary editing does not trip it, but a
# collapse in what gets scanned surfaces immediately. The denominator is printed
# on every run, healthy or not: a count nobody sees until the postmortem is a
# count nobody sees.
readonly MIN_SCRIPTS=12
printf 'shellcheck: %d scripts\n' "${#scripts[@]}"
if [ ${#scripts[@]} -lt "$MIN_SCRIPTS" ]; then
  printf 'SCOPE FAILURE: only %d scripts scanned, expected at least %d\n' \
    "${#scripts[@]}" "$MIN_SCRIPTS" >&2
  exit 2
fi

shellcheck --severity=warning "${scripts[@]}"
