#!/usr/bin/env bash
# Install apt packages with a bounded, retrying wrapper.
#
# Every CI job that needs native libraries ran `apt-get update && apt-get
# install` directly, with no timeout and no retry. An Ubuntu archive mirror
# that stalls therefore consumes the job's entire timeout-minutes budget and
# the job is cancelled mid-apt — indistinguishable, from the checks list, from
# a build or test that genuinely ran too long. Three jobs failed that way in
# one run on 2026-08-18: policy burned 10m07s of a 10m cap, mqtt-integration
# 14m54s of 15m, and msrv 19m57s of 20m, each entirely inside apt.
#
# Raising the caps would have been the wrong response: it extends a hang
# rather than accommodating real work, and it hides the next stall behind a
# longer wall-clock. Bounding each attempt and retrying is the fix — a
# transient mirror failure costs one short attempt instead of the whole job,
# and a genuine outage still fails the job after a known, finite time.
#
# Usage: apt_install.sh <package>...
set -euo pipefail

if (( $# == 0 )); then
  printf '%s\n' 'apt_install.sh: no packages given' >&2
  exit 2
fi

# Per-attempt ceilings. A healthy update+install of this project's native deps
# takes well under a minute on GitHub's runners; these bounds are generous
# enough not to fire on a slow-but-working mirror, and short enough that all
# three attempts plus backoff stay inside the tightest job cap (10m).
readonly update_timeout=120
readonly install_timeout=180
readonly attempts=3

export DEBIAN_FRONTEND=noninteractive

attempt=1
while (( attempt <= attempts )); do
  # `status` is captured in the else branch, where $? is still the condition's
  # exit status. Reading $? after the `if` block instead yields the status of
  # the `if` statement itself — always 0 — which silently disables the timeout
  # detection below and logs every failure as "exit 0".
  if timeout "$update_timeout" sudo -E apt-get update \
    && timeout "$install_timeout" sudo -E apt-get install -y \
      --no-install-recommends "$@"; then
    printf 'apt_install: installed %d package(s) on attempt %d\n' "$#" "$attempt"
    exit 0
  else
    status=$?
  fi
  # `timeout` reports 124 when it kills the child. Distinguishing a stall from
  # a package error matters: a stall is worth retrying against a possibly
  # different mirror, whereas a genuinely missing package will fail the same
  # way three times and the log should say which happened.
  if (( status == 124 )); then
    printf 'apt_install: attempt %d/%d timed out\n' "$attempt" "$attempts" >&2
  else
    printf 'apt_install: attempt %d/%d failed (exit %d)\n' \
      "$attempt" "$attempts" "$status" >&2
  fi

  if (( attempt == attempts )); then
    printf '%s\n' 'apt_install: all attempts exhausted' >&2
    exit 1
  fi

  # Linear backoff. An archive stall is usually brief and often resolves on a
  # different mirror, so waiting long buys little; the point is to not hammer.
  sleep $(( attempt * 10 ))
  attempt=$(( attempt + 1 ))
done
